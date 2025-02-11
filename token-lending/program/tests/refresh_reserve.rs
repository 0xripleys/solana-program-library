#![cfg(feature = "test-bpf")]

mod helpers;

use helpers::*;
use solana_program::{
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
    sysvar,
};
use solana_program_test::*;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
use solend_program::{
    error::LendingError,
    instruction::{refresh_reserve, LendingInstruction},
    math::{Decimal, Rate, TryAdd, TryDiv, TryMul, TrySub},
    processor::process_instruction,
    state::SLOTS_PER_YEAR,
};
use std::str::FromStr;

#[tokio::test]
async fn test_success() {
    let mut test = ProgramTest::new(
        "solend_program",
        solend_program::id(),
        processor!(process_instruction),
    );

    // limit to track compute unit increase
    test.set_compute_max_units(31_000);

    const SOL_RESERVE_LIQUIDITY_LAMPORTS: u64 = 100 * LAMPORTS_TO_SOL;
    const USDC_RESERVE_LIQUIDITY_FRACTIONAL: u64 = 100 * FRACTIONAL_TO_USDC;
    const BORROW_AMOUNT: u64 = 100;

    let user_accounts_owner = Keypair::new();
    let lending_market = add_lending_market(&mut test);

    let mut reserve_config = test_reserve_config();
    reserve_config.loan_to_value_ratio = 80;

    // Configure reserve to a fixed borrow rate of 1%
    const BORROW_RATE: u8 = 1;
    reserve_config.min_borrow_rate = BORROW_RATE;
    reserve_config.optimal_borrow_rate = BORROW_RATE;
    reserve_config.optimal_utilization_rate = 100;

    let usdc_mint = add_usdc_mint(&mut test);
    let usdc_oracle = add_usdc_oracle(&mut test);
    let usdc_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &usdc_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            borrow_amount: BORROW_AMOUNT,
            liquidity_amount: USDC_RESERVE_LIQUIDITY_FRACTIONAL,
            liquidity_mint_decimals: usdc_mint.decimals,
            liquidity_mint_pubkey: usdc_mint.pubkey,
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
        },
    );

    let sol_oracle = add_sol_oracle(&mut test);
    let sol_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &sol_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            borrow_amount: BORROW_AMOUNT,
            liquidity_amount: SOL_RESERVE_LIQUIDITY_LAMPORTS,
            liquidity_mint_decimals: 9,
            liquidity_mint_pubkey: spl_token::native_mint::id(),
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
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
        ],
        Some(&payer.pubkey()),
    );

    transaction.sign(&[&payer], recent_blockhash);
    assert!(banks_client.process_transaction(transaction).await.is_ok());

    let sol_reserve = sol_test_reserve.get_state(&mut banks_client).await;
    let usdc_reserve = usdc_test_reserve.get_state(&mut banks_client).await;

    let slot_rate = Rate::from_percent(BORROW_RATE)
        .try_div(SLOTS_PER_YEAR)
        .unwrap();
    let compound_rate = Rate::one().try_add(slot_rate).unwrap();
    let compound_borrow = Decimal::from(BORROW_AMOUNT).try_mul(compound_rate).unwrap();
    let net_new_debt = compound_borrow
        .try_sub(Decimal::from(BORROW_AMOUNT))
        .unwrap();
    let protocol_take_rate = Rate::from_percent(usdc_reserve.config.protocol_take_rate);
    let delta_accumulated_protocol_fees = net_new_debt.try_mul(protocol_take_rate).unwrap();

    assert_eq!(
        sol_reserve.liquidity.cumulative_borrow_rate_wads,
        compound_rate.into()
    );
    assert_eq!(
        sol_reserve.liquidity.cumulative_borrow_rate_wads,
        usdc_reserve.liquidity.cumulative_borrow_rate_wads
    );
    assert_eq!(sol_reserve.liquidity.borrowed_amount_wads, compound_borrow);
    assert_eq!(
        sol_reserve.liquidity.borrowed_amount_wads,
        usdc_reserve.liquidity.borrowed_amount_wads
    );
    assert_eq!(
        sol_reserve.liquidity.market_price,
        sol_test_reserve.market_price
    );
    assert_eq!(
        usdc_reserve.liquidity.market_price,
        usdc_test_reserve.market_price
    );
    assert_eq!(
        delta_accumulated_protocol_fees,
        usdc_reserve.liquidity.accumulated_protocol_fees_wads
    );
    assert_eq!(
        delta_accumulated_protocol_fees,
        sol_reserve.liquidity.accumulated_protocol_fees_wads
    );
}

#[tokio::test]
async fn test_success_no_switchboard() {
    let mut test = ProgramTest::new(
        "solend_program",
        solend_program::id(),
        processor!(process_instruction),
    );

    // limit to track compute unit increase
    test.set_compute_max_units(31_000);

    const SOL_RESERVE_LIQUIDITY_LAMPORTS: u64 = 100 * LAMPORTS_TO_SOL;
    const USDC_RESERVE_LIQUIDITY_FRACTIONAL: u64 = 100 * FRACTIONAL_TO_USDC;
    const BORROW_AMOUNT: u64 = 100;

    let user_accounts_owner = Keypair::new();
    let lending_market = add_lending_market(&mut test);

    let mut reserve_config = test_reserve_config();
    reserve_config.loan_to_value_ratio = 80;

    // Configure reserve to a fixed borrow rate of 1%
    const BORROW_RATE: u8 = 1;
    reserve_config.min_borrow_rate = BORROW_RATE;
    reserve_config.optimal_borrow_rate = BORROW_RATE;
    reserve_config.optimal_utilization_rate = 100;

    let usdc_mint = add_usdc_mint(&mut test);
    let usdc_oracle = add_usdc_oracle(&mut test);
    let usdc_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &usdc_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            borrow_amount: BORROW_AMOUNT,
            liquidity_amount: USDC_RESERVE_LIQUIDITY_FRACTIONAL,
            liquidity_mint_decimals: usdc_mint.decimals,
            liquidity_mint_pubkey: usdc_mint.pubkey,
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
        },
    );

    let sol_oracle = add_sol_oracle(&mut test);
    let sol_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &sol_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            borrow_amount: BORROW_AMOUNT,
            liquidity_amount: SOL_RESERVE_LIQUIDITY_LAMPORTS,
            liquidity_mint_decimals: 9,
            liquidity_mint_pubkey: spl_token::native_mint::id(),
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
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
            refresh_reserve_no_switchboard(
                solend_program::id(),
                usdc_test_reserve.pubkey,
                usdc_oracle.pyth_price_pubkey,
                false,
            ),
            refresh_reserve_no_switchboard(
                solend_program::id(),
                sol_test_reserve.pubkey,
                sol_oracle.pyth_price_pubkey,
                true,
            ),
        ],
        Some(&payer.pubkey()),
    );

    transaction.sign(&[&payer], recent_blockhash);
    assert!(banks_client.process_transaction(transaction).await.is_ok());

    let sol_reserve = sol_test_reserve.get_state(&mut banks_client).await;
    let usdc_reserve = usdc_test_reserve.get_state(&mut banks_client).await;

    let slot_rate = Rate::from_percent(BORROW_RATE)
        .try_div(SLOTS_PER_YEAR)
        .unwrap();
    let compound_rate = Rate::one().try_add(slot_rate).unwrap();
    let compound_borrow = Decimal::from(BORROW_AMOUNT).try_mul(compound_rate).unwrap();
    let net_new_debt = compound_borrow
        .try_sub(Decimal::from(BORROW_AMOUNT))
        .unwrap();
    let protocol_take_rate = Rate::from_percent(usdc_reserve.config.protocol_take_rate);
    let delta_accumulated_protocol_fees = net_new_debt.try_mul(protocol_take_rate).unwrap();

    assert_eq!(
        sol_reserve.liquidity.cumulative_borrow_rate_wads,
        compound_rate.into()
    );
    assert_eq!(
        sol_reserve.liquidity.cumulative_borrow_rate_wads,
        usdc_reserve.liquidity.cumulative_borrow_rate_wads
    );
    assert_eq!(sol_reserve.liquidity.borrowed_amount_wads, compound_borrow);
    assert_eq!(
        sol_reserve.liquidity.borrowed_amount_wads,
        usdc_reserve.liquidity.borrowed_amount_wads
    );
    assert_eq!(
        sol_reserve.liquidity.market_price,
        sol_test_reserve.market_price
    );
    assert_eq!(
        usdc_reserve.liquidity.market_price,
        usdc_test_reserve.market_price
    );
    assert_eq!(
        delta_accumulated_protocol_fees,
        usdc_reserve.liquidity.accumulated_protocol_fees_wads
    );
    assert_eq!(
        delta_accumulated_protocol_fees,
        sol_reserve.liquidity.accumulated_protocol_fees_wads
    );
}

/// Creates a `RefreshReserve` instruction
pub fn refresh_reserve_no_switchboard(
    program_id: Pubkey,
    reserve_pubkey: Pubkey,
    reserve_liquidity_pyth_oracle_pubkey: Pubkey,
    with_clock: bool,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(reserve_pubkey, false),
        AccountMeta::new_readonly(reserve_liquidity_pyth_oracle_pubkey, false),
    ];
    if with_clock {
        accounts.push(AccountMeta::new_readonly(sysvar::clock::id(), false))
    }
    Instruction {
        program_id,
        accounts,
        data: LendingInstruction::RefreshReserve.pack(),
    }
}

#[tokio::test]
async fn test_pyth_price_stale() {
    let mut test = ProgramTest::new(
        "solend_program",
        solend_program::id(),
        processor!(process_instruction),
    );

    // limit to track compute unit increase
    test.set_compute_max_units(31_000);

    const USDC_RESERVE_LIQUIDITY_FRACTIONAL: u64 = 100 * FRACTIONAL_TO_USDC;

    let user_accounts_owner = Keypair::new();
    let lending_market = add_lending_market(&mut test);

    let reserve_config = test_reserve_config();

    let usdc_mint = add_usdc_mint(&mut test);
    let usdc_oracle = add_usdc_oracle(&mut test);
    let usdc_test_reserve = add_reserve(
        &mut test,
        &lending_market,
        &usdc_oracle,
        &user_accounts_owner,
        AddReserveArgs {
            borrow_amount: 100,
            liquidity_amount: USDC_RESERVE_LIQUIDITY_FRACTIONAL,
            liquidity_mint_decimals: usdc_mint.decimals,
            liquidity_mint_pubkey: usdc_mint.pubkey,
            config: reserve_config,
            slots_elapsed: 238, // elapsed from 1; clock.slot = 239
            ..AddReserveArgs::default()
        },
    );

    let mut test_context = test.start_with_context().await;
    test_context.warp_to_slot(241).unwrap(); // clock.slot = 241

    let ProgramTestContext {
        mut banks_client,
        payer,
        last_blockhash: recent_blockhash,
        ..
    } = test_context;

    let mut transaction = Transaction::new_with_payer(
        &[refresh_reserve(
            solend_program::id(),
            usdc_test_reserve.pubkey,
            usdc_oracle.pyth_price_pubkey,
            Pubkey::from_str(NULL_PUBKEY).unwrap(),
        )],
        Some(&payer.pubkey()),
    );

    transaction.sign(&[&payer], recent_blockhash);
    assert_eq!(
        banks_client
            .process_transaction(transaction)
            .await
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(
            0,
            InstructionError::Custom(LendingError::InvalidOracleConfig as u32),
        ),
    );
}
