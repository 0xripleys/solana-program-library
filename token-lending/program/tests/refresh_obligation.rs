#![cfg(feature = "test-bpf")]

mod helpers;

use helpers::*;
use solana_program_test::*;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solend_program::math::{Rate, TryAdd, TryMul, TrySub};
use solend_program::state::SLOTS_PER_YEAR;
use solend_program::{
    instruction::{refresh_obligation, refresh_reserve},
    math::{Decimal, TryDiv},
    processor::process_instruction,
    state::INITIAL_COLLATERAL_RATIO,
};

#[tokio::test]
async fn test_success() {
    let mut test = ProgramTest::new(
        "solend_program",
        solend_program::id(),
        processor!(process_instruction),
    );

    // limit to track compute unit increase
    test.set_compute_max_units(45_000);

    const SOL_DEPOSIT_AMOUNT: u64 = 100;
    const USDC_BORROW_AMOUNT: u64 = 1_000;
    const SOL_DEPOSIT_AMOUNT_LAMPORTS: u64 =
        SOL_DEPOSIT_AMOUNT * LAMPORTS_TO_SOL * INITIAL_COLLATERAL_RATIO;
    const USDC_BORROW_AMOUNT_FRACTIONAL: u64 = USDC_BORROW_AMOUNT * FRACTIONAL_TO_USDC;
    const SOL_RESERVE_COLLATERAL_LAMPORTS: u64 = 2 * SOL_DEPOSIT_AMOUNT_LAMPORTS;
    const USDC_RESERVE_LIQUIDITY_FRACTIONAL: u64 = 2 * USDC_BORROW_AMOUNT_FRACTIONAL;

    let user_accounts_owner = Keypair::new();
    let lending_market = add_lending_market(&mut test);

    let mut reserve_config = test_reserve_config();
    reserve_config.loan_to_value_ratio = 50;

    // Configure reserve to a fixed borrow rate of 1%
    const BORROW_RATE: u8 = 1;
    reserve_config.min_borrow_rate = BORROW_RATE;
    reserve_config.optimal_borrow_rate = BORROW_RATE;
    reserve_config.optimal_utilization_rate = 100;

    let sol_oracle = add_sol_oracle(&mut test);
    let sol_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &sol_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            collateral_amount: SOL_RESERVE_COLLATERAL_LAMPORTS,
            liquidity_mint_decimals: 9,
            liquidity_mint_pubkey: spl_token::native_mint::id(),
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
        },
    );

    let usdc_mint = add_usdc_mint(&mut test);
    let usdc_oracle = add_usdc_oracle(&mut test);
    let usdc_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &usdc_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            borrow_amount: USDC_BORROW_AMOUNT_FRACTIONAL,
            liquidity_amount: USDC_RESERVE_LIQUIDITY_FRACTIONAL,
            liquidity_mint_decimals: usdc_mint.decimals,
            liquidity_mint_pubkey: usdc_mint.pubkey,
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
        },
    );

    let test_obligation = add_obligation(
        &mut test,
        &lending_market,
        &user_accounts_owner,
        AddObligationArgs {
            deposits: &[(&sol_test_reserve, SOL_DEPOSIT_AMOUNT_LAMPORTS)],
            borrows: &[(&usdc_test_reserve, USDC_BORROW_AMOUNT_FRACTIONAL)],
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddObligationArgs::default()
        },
    );

    let mut test_context = test.start_with_context().await;
    test_context.warp_to_slot(240).unwrap(); // clock.slot = 240

    let ProgramTestContext {
        mut banks_client,
        payer,
        last_blockhash: recent_blockhash,
        ..
    } = test_context;

    let mut transaction = Transaction::new_with_payer(
        &[
            refresh_reserve(
                solend_program::id(),
                usdc_test_reserve.pubkey,
                usdc_oracle.pyth_price_pubkey,
                usdc_oracle.switchboard_feed_pubkey,
            ),
            refresh_reserve(
                solend_program::id(),
                sol_test_reserve.pubkey,
                sol_oracle.pyth_price_pubkey,
                sol_oracle.switchboard_feed_pubkey,
            ),
            refresh_obligation(
                solend_program::id(),
                test_obligation.pubkey,
                vec![sol_test_reserve.pubkey, usdc_test_reserve.pubkey],
            ),
        ],
        Some(&payer.pubkey()),
    );

    transaction.sign(&[&payer], recent_blockhash);
    assert!(banks_client.process_transaction(transaction).await.is_ok());

    let sol_reserve = sol_test_reserve.get_state(&mut banks_client).await;
    let usdc_reserve = usdc_test_reserve.get_state(&mut banks_client).await;
    let obligation = test_obligation.get_state(&mut banks_client).await;

    let collateral = &obligation.deposits[0];
    let liquidity = &obligation.borrows[0];

    let collateral_price = collateral.market_value.try_div(SOL_DEPOSIT_AMOUNT).unwrap();

    let slot_rate = Rate::from_percent(BORROW_RATE)
        .try_div(SLOTS_PER_YEAR)
        .unwrap();
    let compound_rate = Rate::one().try_add(slot_rate).unwrap();
    let compound_borrow = Decimal::from(USDC_BORROW_AMOUNT)
        .try_mul(compound_rate)
        .unwrap();
    let compound_borrow_wads = Decimal::from(USDC_BORROW_AMOUNT_FRACTIONAL)
        .try_mul(compound_rate)
        .unwrap();
    let net_new_debt = compound_borrow_wads
        .try_sub(Decimal::from(USDC_BORROW_AMOUNT_FRACTIONAL))
        .unwrap();
    let protocol_take_rate = Rate::from_percent(usdc_reserve.config.protocol_take_rate);
    let delta_accumulated_protocol_fees = net_new_debt.try_mul(protocol_take_rate).unwrap();
    let new_borrow_amount_wads = Decimal::from(USDC_BORROW_AMOUNT_FRACTIONAL)
        .try_add(net_new_debt)
        .unwrap();

    let liquidity_price = liquidity.market_value.try_div(compound_borrow).unwrap();

    assert_eq!(
        usdc_reserve.liquidity.cumulative_borrow_rate_wads,
        liquidity.cumulative_borrow_rate_wads
    );
    assert_eq!(liquidity.cumulative_borrow_rate_wads, compound_rate.into());
    assert_eq!(
        usdc_reserve.liquidity.borrowed_amount_wads,
        liquidity.borrowed_amount_wads
    );
    assert_eq!(liquidity.borrowed_amount_wads, new_borrow_amount_wads);
    assert_eq!(
        usdc_reserve.liquidity.accumulated_protocol_fees_wads,
        delta_accumulated_protocol_fees
    );
    assert_eq!(
        sol_reserve.liquidity.accumulated_protocol_fees_wads,
        Decimal::from(0u64)
    );
    assert_eq!(sol_reserve.liquidity.market_price, collateral_price,);
    assert_eq!(usdc_reserve.liquidity.market_price, liquidity_price,);
}
