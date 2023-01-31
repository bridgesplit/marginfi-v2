use anchor_lang::{prelude::Clock, InstructionData, ToAccountMetas};
use anchor_spl::token::{self};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use fixtures::prelude::*;
use fixtures::*;
use marginfi::{
    prelude::{MarginfiError, MarginfiGroup},
    state::marginfi_group::{Bank, BankConfig, BankConfigOpt, BankOperationalState},
};
use pretty_assertions::assert_eq;
use solana_program::{
    account_info::IntoAccountInfo, instruction::Instruction, program_pack::Pack, system_program,
};
use solana_program_test::*;
use solana_sdk::{signature::Keypair, signer::Signer, transaction::Transaction};

#[tokio::test]
async fn success_create_marginfi_group() -> anyhow::Result<()> {
    // Setup test executor
    let test_f = TestFixture::new(None).await;

    // Create & initialize marginfi group
    let marginfi_group_key = Keypair::new();

    let accounts = marginfi::accounts::InitializeMarginfiGroup {
        marginfi_group: marginfi_group_key.pubkey(),
        admin: test_f.payer(),
        system_program: system_program::id(),
    };
    let init_marginfi_group_ix = Instruction {
        program_id: marginfi::id(),
        accounts: accounts.to_account_metas(Some(true)),
        data: marginfi::instruction::InitializeMarginfiGroup {}.data(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[init_marginfi_group_ix],
        Some(&test_f.payer().clone()),
        &[&test_f.payer_keypair(), &marginfi_group_key],
        test_f.get_latest_blockhash().await,
    );
    let res = test_f
        .context
        .borrow_mut()
        .banks_client
        .process_transaction(tx)
        .await;
    assert!(res.is_ok());

    // Fetch & deserialize marginfi group account
    let marginfi_group: MarginfiGroup = test_f
        .load_and_deserialize(&marginfi_group_key.pubkey())
        .await;

    // Check basic properties
    assert_eq!(marginfi_group.admin, test_f.payer());

    Ok(())
}

// #[tokio::test]
// async fn success_configure_marginfi_group() {
//     todo!()
// }

#[tokio::test]
async fn success_add_bank() -> anyhow::Result<()> {
    // Setup test executor with non-admin payer
    let test_f = TestFixture::new(None).await;

    let bank_asset_mint_fixture = MintFixture::new(test_f.context.clone(), None, None).await;

    let res = test_f
        .marginfi_group
        .try_lending_pool_add_bank(bank_asset_mint_fixture.key, *DEFAULT_USDC_TEST_BANK_CONFIG)
        .await;
    assert!(res.is_ok());

    // Check bank is active
    let bank = res.unwrap();
    let bank = test_f.try_load(&bank.key).await?;
    assert!(bank.is_some());

    Ok(())
}

#[tokio::test]
async fn failure_add_bank_fake_pyth_feed() -> anyhow::Result<()> {
    // Setup test executor with non-admin payer
    let test_f = TestFixture::new(None).await;

    let bank_asset_mint_fixture = MintFixture::new(test_f.context.clone(), None, None).await;

    let res = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            bank_asset_mint_fixture.key,
            BankConfig {
                oracle_setup: marginfi::state::marginfi_group::OracleSetup::Pyth,
                oracle_keys: create_oracle_key_array(FAKE_PYTH_USDC_FEED),
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await;

    assert!(res.is_err());
    assert_custom_error!(res.unwrap_err(), MarginfiError::InvalidOracleAccount);

    Ok(())
}

#[tokio::test]
async fn success_accrue_interest_rates_1() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let mut bank_config = BankConfig {
        ..*DEFAULT_USDC_TEST_BANK_CONFIG
    };

    bank_config.interest_rate_config.optimal_utilization_rate = I80F48!(0.9).into();
    bank_config.interest_rate_config.plateau_interest_rate = I80F48!(1).into();

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(test_f.usdc_mint.key, bank_config)
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(funding_token_account, &usdc_bank, native!(100, "USDC"))
        .await?;

    let borrower_account = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1000, "SOL"))
        .await;
    borrower_account
        .try_bank_deposit(funding_token_account, &sol_bank, native!(999, "SOL"))
        .await?;

    let destination_account = test_f.usdc_mint.create_token_account_and_mint_to(0).await;
    borrower_account
        .try_bank_borrow(destination_account, &usdc_bank, native!(90, "USDC"))
        .await?;

    {
        let mut ctx = test_f.context.borrow_mut();
        let mut clock: Clock = ctx.banks_client.get_sysvar().await?;
        // Advance clock by 1 year
        clock.unix_timestamp += 365 * 24 * 60 * 60;
        ctx.set_sysvar(&clock);
    }

    test_f
        .marginfi_group
        .try_accrue_interest(&usdc_bank)
        .await?;

    let borrower_mfi_account = borrower_account.load().await;
    let borrower_bank_account = borrower_mfi_account.lending_account.balances[1];
    let usdc_bank: Bank = usdc_bank.load().await;
    let liabilities =
        usdc_bank.get_liability_amount(borrower_bank_account.liability_shares.into())?;

    let lender_mfi_account = lender_mfi_account_f.load().await;
    let lender_bank_account = lender_mfi_account.lending_account.balances[0];
    let assets = usdc_bank.get_asset_amount(lender_bank_account.asset_shares.into())?;

    assert_eq_noise!(
        liabilities,
        I80F48::from(native!(180, "USDC")),
        I80F48!(100)
    );
    assert_eq_noise!(assets, I80F48::from(native!(190, "USDC")), I80F48!(100));

    Ok(())
}

#[tokio::test]
async fn success_accrue_interest_rates_2() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let mut bank_config = BankConfig {
        max_capacity: native!(1_000_000_000, "USDC"),
        ..*DEFAULT_USDC_TEST_BANK_CONFIG
    };

    bank_config.interest_rate_config.optimal_utilization_rate = I80F48!(0.9).into();
    bank_config.interest_rate_config.plateau_interest_rate = I80F48!(1).into();
    bank_config.interest_rate_config.protocol_fixed_fee_apr = I80F48!(0.01).into();
    bank_config.interest_rate_config.insurance_fee_fixed_apr = I80F48!(0.01).into();

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(test_f.usdc_mint.key, bank_config)
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                max_capacity: native!(200_000_000, "SOL"),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000_000, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(
            funding_token_account,
            &usdc_bank,
            native!(100_000_000, "USDC"),
        )
        .await?;

    let borrower_account = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(10_000_000, "SOL"))
        .await;
    borrower_account
        .try_bank_deposit(funding_token_account, &sol_bank, native!(10_000_000, "SOL"))
        .await?;

    let destination_account = test_f.usdc_mint.create_token_account_and_mint_to(0).await;
    borrower_account
        .try_bank_borrow(destination_account, &usdc_bank, native!(90_000_000, "USDC"))
        .await?;

    {
        let mut ctx = test_f.context.borrow_mut();
        let mut clock: Clock = ctx.banks_client.get_sysvar().await?;
        // Advance clock by 1 year
        clock.unix_timestamp += 60;
        ctx.set_sysvar(&clock);
    }

    test_f
        .marginfi_group
        .try_accrue_interest(&usdc_bank)
        .await?;

    test_f.marginfi_group.try_collect_fees(&usdc_bank).await?;

    let borrower_mfi_account = borrower_account.load().await;
    let borrower_bank_account = borrower_mfi_account.lending_account.balances[1];
    let usdc_bank = usdc_bank.load().await;
    let liabilities =
        usdc_bank.get_liability_amount(borrower_bank_account.liability_shares.into())?;

    let lender_mfi_account = lender_mfi_account_f.load().await;
    let lender_bank_account = lender_mfi_account.lending_account.balances[0];
    let assets = usdc_bank.get_asset_amount(lender_bank_account.asset_shares.into())?;

    assert_eq_noise!(liabilities, I80F48!(90000174657530), I80F48!(10));
    assert_eq_noise!(assets, I80F48!(100000171232862), I80F48!(10));

    let mut ctx = test_f.context.borrow_mut();
    let protocol_fees = ctx
        .banks_client
        .get_account(usdc_bank.fee_vault)
        .await?
        .unwrap();
    let insurance_fees = ctx
        .banks_client
        .get_account(usdc_bank.insurance_vault)
        .await?
        .unwrap();

    let protocol_fees =
        token::spl_token::state::Account::unpack_from_slice(protocol_fees.data.as_slice())?;
    let insurance_fees =
        token::spl_token::state::Account::unpack_from_slice(insurance_fees.data.as_slice())?;

    assert_eq!(protocol_fees.amount, 1712326);
    assert_eq!(insurance_fees.amount, 1712326);

    Ok(())
}

#[tokio::test]
async fn lending_pool_handle_bankruptcy_failure_not_bankrupt() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(lender_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let borrower_mfi_account_f = test_f.create_marginfi_account().await;
    let borrower_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1_001, "SOL"))
        .await;

    borrower_mfi_account_f
        .try_bank_deposit(borrower_token_account_sol, &sol_bank, native!(1_001, "SOL"))
        .await?;

    let borrower_token_account_usdc = test_f.usdc_mint.create_token_account_and_mint_to(0).await;

    borrower_mfi_account_f
        .try_bank_borrow(
            borrower_token_account_usdc,
            &usdc_bank,
            native!(10_000, "USDC"),
        )
        .await?;

    let res = test_f
        .marginfi_group
        .try_handle_bankruptcy(&usdc_bank, &borrower_mfi_account_f)
        .await;

    assert!(res.is_err());
    assert_custom_error!(res.unwrap_err(), MarginfiError::AccountNotBankrupt);

    Ok(())
}

#[tokio::test]
async fn lending_pool_handle_bankruptcy_success_no_debt() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_token_account_usdc = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(
            lender_token_account_usdc,
            &usdc_bank,
            native!(100_000, "USDC"),
        )
        .await?;

    let borrower_mfi_account_f = test_f.create_marginfi_account().await;
    let borrower_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1_001, "SOL"))
        .await;

    borrower_mfi_account_f
        .try_bank_deposit(borrower_token_account_sol, &sol_bank, native!(1_001, "SOL"))
        .await?;

    let borrower_token_account_usdc = test_f.usdc_mint.create_token_account_and_mint_to(0).await;

    borrower_mfi_account_f
        .try_bank_borrow(
            borrower_token_account_usdc,
            &usdc_bank,
            native!(10_000, "USDC"),
        )
        .await?;

    let mut borrower_mfi_account = borrower_mfi_account_f.load().await;
    borrower_mfi_account.lending_account.balances[0]
        .asset_shares
        .value = 0;

    borrower_mfi_account_f
        .set_account(&borrower_mfi_account)
        .await?;

    let res = test_f
        .marginfi_group
        .try_handle_bankruptcy(&sol_bank, &borrower_mfi_account_f)
        .await;

    assert!(res.is_err());
    assert_custom_error!(res.unwrap_err(), MarginfiError::BalanceNotBadDebt);

    Ok(())
}

#[tokio::test]
async fn lending_pool_handle_bankruptcy_success_fully_insured() -> anyhow::Result<()> {
    let mut test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(funding_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let borrower_account = test_f.create_marginfi_account().await;
    let borrower_deposit_account = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1_001, "SOL"))
        .await;

    borrower_account
        .try_bank_deposit(borrower_deposit_account, &sol_bank, native!(1_001, "SOL"))
        .await?;

    let borrower_borrow_account = test_f.usdc_mint.create_token_account_and_mint_to(0).await;

    borrower_account
        .try_bank_borrow(borrower_borrow_account, &usdc_bank, native!(10_000, "USDC"))
        .await?;

    let mut borrower_mfi_account = borrower_account.load().await;
    borrower_mfi_account.lending_account.balances[0]
        .asset_shares
        .value = 0;

    borrower_account.set_account(&borrower_mfi_account).await?;

    test_f
        .usdc_mint
        .mint_to(
            &usdc_bank.load().await.insurance_vault,
            native!(10_000, "USDC"),
        )
        .await;

    test_f
        .marginfi_group
        .try_handle_bankruptcy(&usdc_bank, &borrower_account)
        .await?;

    let borrower_mfi_account = borrower_account.load().await;
    let borrower_usdc_balance = borrower_mfi_account.lending_account.balances[1];

    assert_eq!(
        I80F48::from(borrower_usdc_balance.liability_shares),
        I80F48::ZERO
    );

    let lender_mfi_account = lender_mfi_account_f.load().await;
    let usdc_bank = usdc_bank.load().await;

    let lender_usdc_value = usdc_bank.get_asset_amount(
        lender_mfi_account.lending_account.balances[0]
            .asset_shares
            .into(),
    )?;

    assert_eq_noise!(
        lender_usdc_value,
        I80F48::from(native!(100_000, "USDC")),
        I80F48::ONE
    );

    let insurance_amount = token::accessor::amount(
        &(
            &usdc_bank.insurance_vault,
            &mut test_f
                .context
                .borrow_mut()
                .banks_client
                .get_account(usdc_bank.insurance_vault)
                .await?
                .unwrap(),
        )
            .into_account_info(),
    )?;

    assert_eq!(insurance_amount, 0);

    Ok(())
}

#[tokio::test]
async fn lending_pool_handle_bankruptcy_success_partially_insured() -> anyhow::Result<()> {
    let mut test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(funding_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let borrower_account = test_f.create_marginfi_account().await;
    let borrower_deposit_account = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1_001, "SOL"))
        .await;

    borrower_account
        .try_bank_deposit(borrower_deposit_account, &sol_bank, native!(1_001, "SOL"))
        .await?;

    let borrower_borrow_account = test_f.usdc_mint.create_token_account_and_mint_to(0).await;

    borrower_account
        .try_bank_borrow(borrower_borrow_account, &usdc_bank, native!(10_000, "USDC"))
        .await?;

    let mut borrower_mfi_account = borrower_account.load().await;
    borrower_mfi_account.lending_account.balances[0]
        .asset_shares
        .value = 0;

    borrower_account.set_account(&borrower_mfi_account).await?;

    test_f
        .usdc_mint
        .mint_to(
            &usdc_bank.load().await.insurance_vault,
            native!(5_000, "USDC"),
        )
        .await;

    test_f
        .marginfi_group
        .try_handle_bankruptcy(&usdc_bank, &borrower_account)
        .await?;

    let borrower_mfi_account = borrower_account.load().await;
    let borrower_usdc_balance = borrower_mfi_account.lending_account.balances[1];

    assert_eq!(
        I80F48::from(borrower_usdc_balance.liability_shares),
        I80F48::ZERO
    );

    let lender_mfi_account = lender_mfi_account_f.load().await;
    let usdc_bank = usdc_bank.load().await;

    let lender_usdc_value = usdc_bank.get_asset_amount(
        lender_mfi_account.lending_account.balances[0]
            .asset_shares
            .into(),
    )?;

    assert_eq_noise!(
        lender_usdc_value,
        I80F48::from(native!(95_000, "USDC")),
        I80F48::ONE
    );

    let insurance_amount = token::accessor::amount(
        &(
            &usdc_bank.insurance_vault,
            &mut test_f
                .context
                .borrow_mut()
                .banks_client
                .get_account(usdc_bank.insurance_vault)
                .await?
                .unwrap(),
        )
            .into_account_info(),
    )?;

    assert_eq!(insurance_amount, 0);

    Ok(())
}

#[tokio::test]
async fn lending_pool_handle_bankruptcy_success_not_insured() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let funding_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_mfi_account_f
        .try_bank_deposit(funding_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let borrower_account = test_f.create_marginfi_account().await;
    let borrower_deposit_account = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1_001, "SOL"))
        .await;

    borrower_account
        .try_bank_deposit(borrower_deposit_account, &sol_bank, native!(1_001, "SOL"))
        .await?;

    let borrower_borrow_account = test_f.usdc_mint.create_token_account_and_mint_to(0).await;

    borrower_account
        .try_bank_borrow(borrower_borrow_account, &usdc_bank, native!(10_000, "USDC"))
        .await?;

    let mut borrower_mfi_account = borrower_account.load().await;
    borrower_mfi_account.lending_account.balances[0]
        .asset_shares
        .value = 0;

    borrower_account.set_account(&borrower_mfi_account).await?;

    test_f
        .marginfi_group
        .try_handle_bankruptcy(&usdc_bank, &borrower_account)
        .await?;

    let borrower_mfi_account = borrower_account.load().await;
    let borrower_usdc_balance = borrower_mfi_account.lending_account.balances[1];

    assert_eq!(
        I80F48::from(borrower_usdc_balance.liability_shares),
        I80F48::ZERO
    );

    let lender_mfi_account = lender_mfi_account_f.load().await;
    let usdc_bank = usdc_bank.load().await;

    let lender_usdc_value = usdc_bank.get_asset_amount(
        lender_mfi_account.lending_account.balances[0]
            .asset_shares
            .into(),
    )?;

    assert_eq_noise!(
        lender_usdc_value,
        I80F48::from(native!(90_000, "USDC")),
        I80F48::ONE
    );

    Ok(())
}

#[tokio::test]
async fn lending_pool_handle_bankruptcy_success_not_insured_3_depositors() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_1_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_1_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_1_mfi_account_f
        .try_bank_deposit(lender_1_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let lender_2_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_2_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_2_mfi_account_f
        .try_bank_deposit(lender_2_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let lender_3_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_3_token_account = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_3_mfi_account_f
        .try_bank_deposit(lender_3_token_account, &usdc_bank, native!(100_000, "USDC"))
        .await?;

    let borrower_mfi_account_f = test_f.create_marginfi_account().await;
    let borrower_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(1_001, "SOL"))
        .await;

    borrower_mfi_account_f
        .try_bank_deposit(borrower_token_account_sol, &sol_bank, native!(1_001, "SOL"))
        .await?;

    let borrower_token_account_usdc = test_f.usdc_mint.create_token_account_and_mint_to(0).await;

    borrower_mfi_account_f
        .try_bank_borrow(
            borrower_token_account_usdc,
            &usdc_bank,
            native!(10_000, "USDC"),
        )
        .await?;

    let mut borrower_mfi_account = borrower_mfi_account_f.load().await;
    borrower_mfi_account.lending_account.balances[0]
        .asset_shares
        .value = 0;

    borrower_mfi_account_f
        .set_account(&borrower_mfi_account)
        .await?;

    test_f
        .marginfi_group
        .try_handle_bankruptcy(&usdc_bank, &borrower_mfi_account_f)
        .await?;

    let borrower_mfi_account = borrower_mfi_account_f.load().await;
    let borrower_usdc_balance = borrower_mfi_account.lending_account.balances[1];

    assert_eq!(
        I80F48::from(borrower_usdc_balance.liability_shares),
        I80F48::ZERO
    );

    let lender_1_mfi_account = lender_1_mfi_account_f.load().await;
    let usdc_bank = usdc_bank.load().await;

    let lender_usdc_value = usdc_bank.get_asset_amount(
        lender_1_mfi_account.lending_account.balances[0]
            .asset_shares
            .into(),
    )?;

    assert_eq_noise!(
        lender_usdc_value,
        I80F48::from(native!(96_666, "USDC")),
        I80F48::from(native!(1, "USDC"))
    );

    Ok(())
}

#[tokio::test]
async fn lending_pool_bank_paused_should_error() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    test_f
        .marginfi_group
        .try_lending_pool_configure_bank(
            &usdc_bank,
            BankConfigOpt {
                operational_state: Some(BankOperationalState::Paused),
                ..BankConfigOpt::default()
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_token_account_usdc = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    let res = lender_mfi_account_f
        .try_bank_deposit(
            lender_token_account_usdc,
            &usdc_bank,
            native!(100_000, "USDC"),
        )
        .await;

    assert!(res.is_err());
    assert_custom_error!(res.unwrap_err(), MarginfiError::BankPaused);

    Ok(())
}

#[tokio::test]
async fn lending_pool_bank_reduce_only_success_withdraw() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_token_account_sol = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;

    lender_mfi_account_f
        .try_bank_deposit(
            lender_token_account_sol,
            &usdc_bank,
            native!(100_000, "USDC"),
        )
        .await?;

    test_f
        .marginfi_group
        .try_lending_pool_configure_bank(
            &usdc_bank,
            BankConfigOpt {
                operational_state: Some(BankOperationalState::ReduceOnly),
                ..BankConfigOpt::default()
            },
        )
        .await?;

    let res = lender_mfi_account_f
        .try_bank_withdraw(
            lender_token_account_sol,
            &usdc_bank,
            native!(100_000, "USDC"),
            None,
        )
        .await;

    assert!(res.is_ok());

    Ok(())
}

#[tokio::test]
async fn lending_pool_bank_reduce_only_borrow_failure() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_1_mfi_account = test_f.create_marginfi_account().await;
    let lender_1_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(100, "SOL"))
        .await;
    lender_1_mfi_account
        .try_bank_deposit(lender_1_token_account_sol, &sol_bank, native!(100, "SOL"))
        .await?;

    let lender_2_mfi_account = test_f.create_marginfi_account().await;
    let lender_2_token_account_usdc = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_2_mfi_account
        .try_bank_deposit(
            lender_2_token_account_usdc,
            &usdc_bank,
            native!(100_000, "USDC"),
        )
        .await?;

    test_f
        .marginfi_group
        .try_lending_pool_configure_bank(
            &sol_bank,
            BankConfigOpt {
                operational_state: Some(BankOperationalState::ReduceOnly),
                ..BankConfigOpt::default()
            },
        )
        .await?;

    let lender_2_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(0, "SOL"))
        .await;
    let res = lender_2_mfi_account
        .try_bank_borrow(lender_2_token_account_sol, &sol_bank, native!(1, "SOL"))
        .await;

    assert!(res.is_err());
    assert_custom_error!(res.unwrap_err(), MarginfiError::BankReduceOnly);

    Ok(())
}

#[tokio::test]
async fn lending_pool_bank_reduce_only_deposit_failure() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    test_f
        .marginfi_group
        .try_lending_pool_configure_bank(
            &usdc_bank,
            BankConfigOpt {
                operational_state: Some(BankOperationalState::ReduceOnly),
                ..BankConfigOpt::default()
            },
        )
        .await?;

    let lender_mfi_account_f = test_f.create_marginfi_account().await;
    let lender_token_account_usdc = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;

    let res = lender_mfi_account_f
        .try_bank_deposit(
            lender_token_account_usdc,
            &usdc_bank,
            native!(100_000, "USDC"),
        )
        .await;

    assert!(res.is_err());
    assert_custom_error!(res.unwrap_err(), MarginfiError::BankReduceOnly);

    Ok(())
}

#[tokio::test]
async fn lending_pool_bank_reduce_only_success_deposit() -> anyhow::Result<()> {
    let test_f = TestFixture::new(None).await;

    let usdc_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.usdc_mint.key,
            BankConfig {
                ..*DEFAULT_USDC_TEST_BANK_CONFIG
            },
        )
        .await?;

    let sol_bank = test_f
        .marginfi_group
        .try_lending_pool_add_bank(
            test_f.sol_mint.key,
            BankConfig {
                asset_weight_init: I80F48!(1).into(),
                ..*DEFAULT_SOL_TEST_BANK_CONFIG
            },
        )
        .await?;

    let lender_1_mfi_account = test_f.create_marginfi_account().await;
    let lender_1_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(100, "SOL"))
        .await;
    lender_1_mfi_account
        .try_bank_deposit(lender_1_token_account_sol, &sol_bank, native!(100, "SOL"))
        .await?;

    let lender_2_mfi_account = test_f.create_marginfi_account().await;
    let lender_2_token_account_usdc = test_f
        .usdc_mint
        .create_token_account_and_mint_to(native!(100_000, "USDC"))
        .await;
    lender_2_mfi_account
        .try_bank_deposit(
            lender_2_token_account_usdc,
            &usdc_bank,
            native!(100_000, "USDC"),
        )
        .await?;

    let lender_2_token_account_sol = test_f
        .sol_mint
        .create_token_account_and_mint_to(native!(0, "SOL"))
        .await;
    lender_2_mfi_account
        .try_bank_borrow(lender_2_token_account_sol, &sol_bank, native!(1, "SOL"))
        .await?;

    test_f
        .marginfi_group
        .try_lending_pool_configure_bank(
            &usdc_bank,
            BankConfigOpt {
                operational_state: Some(BankOperationalState::ReduceOnly),
                ..BankConfigOpt::default()
            },
        )
        .await?;
    let res = lender_2_mfi_account
        .try_bank_repay(
            lender_2_token_account_sol,
            &sol_bank,
            native!(1, "SOL"),
            None,
        )
        .await;

    assert!(res.is_ok());

    Ok(())
}
