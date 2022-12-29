use super::marginfi_account::WeightType;
use crate::{
    check,
    constants::{
        FEE_VAULT_AUTHORITY_SEED, FEE_VAULT_SEED, INSURANCE_VAULT_AUTHORITY_SEED,
        INSURANCE_VAULT_SEED, LIQUIDITY_VAULT_AUTHORITY_SEED, LIQUIDITY_VAULT_SEED, PYTH_ID,
        SECONDS_PER_YEAR,
    },
    math_error,
    prelude::MarginfiError,
    set_if_some, MarginfiResult,
};
use anchor_lang::prelude::*;
use anchor_spl::token::{transfer, Transfer};
use fixed::types::I80F48;
use pyth_sdk_solana::{load_price_feed_from_account_info, PriceFeed};
use std::collections::BTreeMap;

#[account(zero_copy)]
#[cfg_attr(
    any(feature = "test", feature = "client"),
    derive(Debug, PartialEq, Eq)
)]
#[derive(Default)]
pub struct MarginfiGroup {
    pub admin: Pubkey,
}

impl MarginfiGroup {
    /// Configure the group parameters.
    /// This function validates config values so the group remains in a valid state.
    /// Any modification of group config should happen through this function.
    pub fn configure(&mut self, config: GroupConfig) -> MarginfiResult {
        set_if_some!(self.admin, config.admin);

        Ok(())
    }

    /// Set the group parameters when initializing a group.
    /// This should be called only when the group is first initialized.
    /// Both margin requirements are initially set to 100% and should be configured before use.
    #[allow(clippy::too_many_arguments)]
    pub fn set_initial_configuration(&mut self, admin_pk: Pubkey) {
        self.admin = admin_pk;
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Default)]
pub struct GroupConfig {
    pub admin: Option<Pubkey>,
}

pub fn load_pyth_price_feed(ai: &AccountInfo) -> MarginfiResult<PriceFeed> {
    check!(ai.owner.eq(&PYTH_ID), MarginfiError::InvalidPythAccount);
    let price_feed =
        load_price_feed_from_account_info(ai).map_err(|_| MarginfiError::InvalidPythAccount)?;
    Ok(price_feed)
}
#[cfg_attr(
    any(feature = "test", feature = "client"),
    derive(Debug, PartialEq, Eq)
)]
#[zero_copy]
#[derive(Default, AnchorDeserialize, AnchorSerialize)]
pub struct InterestRateConfig {
    // Curve Params
    pub optimal_utilization_rate: WrappedI80F48,
    pub plateau_interest_rate: WrappedI80F48,
    pub max_interest_rate: WrappedI80F48,

    // Fees
    pub insurance_fee_fixed_apr: WrappedI80F48,
    pub insurance_ir_fee: WrappedI80F48,
    pub protocol_fixed_fee_apr: WrappedI80F48,
    pub protocol_ir_fee: WrappedI80F48,
}

impl InterestRateConfig {
    /// Return interest rate charged to borrowers and to depositors.
    /// Rate is denominated in APR (0-).
    ///
    /// Return (`lending_rate`, `borrowing_rate`, `group_fees_apr`, `insurance_fees_apr`)
    pub fn calc_interest_rate(
        &self,
        utilization_ratio: I80F48,
    ) -> Option<(I80F48, I80F48, I80F48, I80F48)> {
        let protocol_ir_fee = I80F48::from(self.protocol_ir_fee);
        let insurance_ir_fee = I80F48::from(self.insurance_ir_fee);

        let protocol_fixed_fee_apr = I80F48::from(self.protocol_fixed_fee_apr);
        let insurance_fee_fixed_apr = I80F48::from(self.insurance_fee_fixed_apr);

        let rate_fee = protocol_ir_fee + insurance_ir_fee;
        let total_fixed_fee_apr = protocol_fixed_fee_apr + insurance_fee_fixed_apr;

        let base_rate = self.interest_rate_curve(utilization_ratio)?;

        // Lending rate is adjusted for utilization ratio to symmetrize payments between borrowers and depositors.
        let lending_rate = base_rate.checked_mul(utilization_ratio)?;

        // Borrowing rate is adjusted for fees.
        // borrowing_rate = base_rate + base_rate * rate_fee + total_fixed_fee_apr
        let borrowing_rate = base_rate
            .checked_mul(I80F48::ONE.checked_add(rate_fee)?)?
            .checked_add(total_fixed_fee_apr)?;

        let group_fees_apr = calc_fee_rate(
            base_rate,
            self.protocol_ir_fee.into(),
            self.protocol_fixed_fee_apr.into(),
        )?;

        let insurance_fees_apr = calc_fee_rate(
            base_rate,
            self.insurance_ir_fee.into(),
            self.insurance_fee_fixed_apr.into(),
        )?;

        assert!(lending_rate >= I80F48::ZERO);
        assert!(borrowing_rate >= I80F48::ZERO);
        assert!(group_fees_apr >= I80F48::ZERO);
        assert!(insurance_fees_apr >= I80F48::ZERO);

        Some((
            lending_rate,
            borrowing_rate,
            group_fees_apr,
            insurance_fees_apr,
        ))
    }

    /// Piecewise linear interest rate function.
    /// The curves approaches the `plateau_interest_rate` as the utilization ratio approaches the `optimal_utilization_rate`,
    /// once the utilization ratio exceeds the `optimal_utilization_rate`, the curve approaches the `max_interest_rate`.
    ///
    /// To be clear we don't particularly appreciate the piecewise linear nature of this "curve", but it is what it is.
    #[inline]
    fn interest_rate_curve(&self, ur: I80F48) -> Option<I80F48> {
        let optimal_ur = self.optimal_utilization_rate.into();
        let plateau_ir = self.plateau_interest_rate.into();
        let max_ir: I80F48 = self.max_interest_rate.into();

        if ur <= optimal_ur {
            ur.checked_div(optimal_ur)?.checked_mul(plateau_ir)
        } else {
            (ur - optimal_ur)
                .checked_div(I80F48::ONE - optimal_ur)?
                .checked_mul(max_ir - plateau_ir)?
                .checked_add(plateau_ir)
        }
    }
}

#[account(zero_copy)]
#[cfg_attr(
    any(feature = "test", feature = "client"),
    derive(Debug, PartialEq, Eq)
)]
#[derive(Default)]
pub struct Bank {
    pub mint_pk: Pubkey,
    pub group: Pubkey,

    pub deposit_share_value: WrappedI80F48,
    pub liability_share_value: WrappedI80F48,

    pub liquidity_vault: Pubkey,
    pub insurance_vault: Pubkey,
    pub fee_vault: Pubkey,

    pub config: BankConfig,

    pub total_borrow_shares: WrappedI80F48,
    pub total_deposit_shares: WrappedI80F48,

    pub last_update: i64,
}

impl Bank {
    pub fn new(
        marginfi_group_pk: Pubkey,
        config: BankConfig,
        mint_pk: Pubkey,
        liquidity_vault: Pubkey,
        insurance_vault: Pubkey,
        fee_vault: Pubkey,
        current_timestamp: i64,
    ) -> Bank {
        Bank {
            mint_pk,
            deposit_share_value: I80F48::ONE.into(),
            liability_share_value: I80F48::ONE.into(),
            liquidity_vault,
            insurance_vault,
            fee_vault,
            config,
            total_borrow_shares: I80F48::ZERO.into(),
            total_deposit_shares: I80F48::ZERO.into(),
            last_update: current_timestamp,
            group: marginfi_group_pk,
        }
    }

    #[inline]
    pub fn load_from_account_info(bank_account_ai: &AccountInfo) -> MarginfiResult<Self> {
        let bank = *bytemuck::from_bytes::<Bank>(&bank_account_ai.data.borrow());
        Ok(bank)
    }

    pub fn get_liability_amount(&self, shares: I80F48) -> MarginfiResult<I80F48> {
        Ok(shares
            .checked_mul(self.liability_share_value.into())
            .ok_or_else(math_error!())?)
    }

    pub fn get_deposit_amount(&self, shares: I80F48) -> MarginfiResult<I80F48> {
        Ok(shares
            .checked_mul(self.deposit_share_value.into())
            .ok_or_else(math_error!())?)
    }

    pub fn get_liability_shares(&self, value: I80F48) -> MarginfiResult<I80F48> {
        Ok(value
            .checked_div(self.liability_share_value.into())
            .ok_or_else(math_error!())?)
    }

    pub fn get_deposit_shares(&self, value: I80F48) -> MarginfiResult<I80F48> {
        Ok(value
            .checked_div(self.deposit_share_value.into())
            .ok_or_else(math_error!())?)
    }

    pub fn change_deposit_shares(&mut self, shares: I80F48) -> MarginfiResult {
        let total_deposit_shares: I80F48 = self.total_deposit_shares.into();
        self.total_deposit_shares = total_deposit_shares
            .checked_add(shares)
            .ok_or_else(math_error!())?
            .into();

        if shares.is_positive() {
            let total_shares_value = self.get_deposit_amount(self.total_deposit_shares.into())?;
            let max_deposit_capacity = self.get_deposit_amount(self.config.max_capacity.into())?;

            check!(
                total_shares_value < max_deposit_capacity,
                crate::prelude::MarginfiError::BankDepositCapacityExceeded
            )
        }

        Ok(())
    }

    pub fn change_liability_shares(&mut self, shares: I80F48) -> MarginfiResult {
        let total_borrow_shares: I80F48 = self.total_borrow_shares.into();
        self.total_borrow_shares = total_borrow_shares
            .checked_add(shares)
            .ok_or_else(math_error!())?
            .into();
        Ok(())
    }

    pub fn configure(&mut self, config: BankConfigOpt) -> MarginfiResult {
        set_if_some!(self.config.deposit_weight_init, config.deposit_weight_init);
        set_if_some!(
            self.config.deposit_weight_maint,
            config.deposit_weight_maint
        );
        set_if_some!(
            self.config.liability_weight_init,
            config.liability_weight_init
        );
        set_if_some!(
            self.config.liability_weight_maint,
            config.liability_weight_maint
        );
        set_if_some!(self.config.max_capacity, config.max_capacity);
        set_if_some!(self.config.pyth_oracle, config.pyth_oracle);
        Ok(())
    }

    #[inline]
    pub fn load_price_feed(
        &self,
        pyth_account_map: &BTreeMap<Pubkey, &AccountInfo>,
    ) -> MarginfiResult<PriceFeed> {
        let pyth_account = pyth_account_map
            .get(&self.config.pyth_oracle)
            .ok_or(MarginfiError::MissingPythAccount)?;

        Ok(load_price_feed_from_account_info(pyth_account)
            .map_err(|_| MarginfiError::InvalidPythAccount)?)
    }

    #[inline]
    pub fn load_price_feed_from_account_info(&self, ai: &AccountInfo) -> MarginfiResult<PriceFeed> {
        check!(
            self.config.pyth_oracle.eq(ai.key),
            MarginfiError::InvalidPythAccount
        );
        let pyth_account =
            load_price_feed_from_account_info(ai).map_err(|_| MarginfiError::InvalidPythAccount)?;

        Ok(pyth_account)
    }

    pub fn accrue_interest(&mut self, clock: &Clock) -> MarginfiResult<(u64, u64)> {
        let time_delta: u64 = (clock.unix_timestamp - self.last_update)
            .try_into()
            .unwrap();

        let total_deposits = self.get_deposit_amount(self.total_deposit_shares.into())?;
        let total_liabilities = self.get_liability_amount(self.total_borrow_shares.into())?;

        let (
            deposit_share_value,
            liability_share_value,
            group_fees_collected,
            insurance_fees_collected,
        ) = calc_interest_rate_accrual_state_changes(
            time_delta,
            total_deposits,
            total_liabilities,
            &self.config.interest_rate_config,
            self.deposit_share_value.into(),
            self.liability_share_value.into(),
        )
        .ok_or_else(math_error!())?;

        self.deposit_share_value = deposit_share_value.into();
        self.liability_share_value = liability_share_value.into();
        self.last_update = clock.unix_timestamp;

        Ok((
            group_fees_collected.to_num(),
            insurance_fees_collected.to_num(),
        ))
    }

    pub fn deposit_spl_transfer<'b: 'c, 'c: 'b>(
        &self,
        amount: u64,
        accounts: Transfer<'b>,
        program: AccountInfo<'c>,
    ) -> MarginfiResult {
        check!(
            accounts.to.key.eq(&self.liquidity_vault),
            MarginfiError::InvalidTransfer
        );

        msg!(
            "deposit_spl_transfer: amount: {} from {} to {}, auth {}",
            amount,
            accounts.from.key,
            accounts.to.key,
            accounts.authority.key
        );

        transfer(CpiContext::new(program, accounts), amount)
    }

    pub fn withdraw_spl_transfer<'b: 'c, 'c: 'b>(
        &self,
        amount: u64,
        accounts: Transfer<'b>,
        program: AccountInfo<'c>,
        signer_seeds: &[&[&[u8]]],
    ) -> MarginfiResult {
        msg!(
            "withdraw_spl_transfer: amount: {} from {} to {}, auth {}",
            amount,
            accounts.from.key,
            accounts.to.key,
            accounts.authority.key
        );

        transfer(
            CpiContext::new_with_signer(program, accounts, signer_seeds),
            amount,
        )
    }

    pub fn socialize_loss(&mut self, loss_amount: I80F48) -> MarginfiResult {
        let n_shares: I80F48 = self.total_deposit_shares.into();
        let old_share_value: I80F48 = self.deposit_share_value.into();

        let new_share_value = n_shares
            .checked_mul(old_share_value)
            .ok_or_else(math_error!())?
            .checked_sub(loss_amount)
            .ok_or_else(math_error!())?
            .checked_div(n_shares)
            .ok_or_else(math_error!())?;

        self.deposit_share_value = new_share_value.into();

        Ok(())
    }
}

/// We use a simple interest rate model that auto settles the accrued interest into the lending account balances.
/// The plan is to move to a compound interest model in the future.
///
/// Simple interest rate model:
/// - `P` - principal
/// - `i` - interest rate (per second)
/// - `t` - time (in seconds)
///
/// `P_t = P_0 * (1 + i) * t`
///
/// We use two interest rates, one for lending and one for borrowing.
///
/// Lending interest rate:
/// - `i_l` - lending interest rate
/// - `i` - base interest rate
/// - `ur` - utilization rate
///
/// `i_l` = `i` * `ur`
///
/// Borrowing interest rate:
/// - `i_b` - borrowing interest rate
/// - `i` - base interest rate
/// - `f_i` - interest rate fee
/// - `f_f` - fixed fee
///
/// `i_b = i * (1 + f_i) + f_f`
///
fn calc_interest_rate_accrual_state_changes(
    time_delta: u64,
    total_deposits: I80F48,
    total_liabilities: I80F48,
    interest_rate_config: &InterestRateConfig,
    deposit_share_value: I80F48,
    liability_share_value: I80F48,
) -> Option<(I80F48, I80F48, I80F48, I80F48)> {
    let utilization_rate = total_liabilities.checked_div(total_deposits)?;
    let (lending_apr, borrowing_apr, group_fee_apr, insurance_fee_apr) =
        interest_rate_config.calc_interest_rate(utilization_rate)?;

    msg!(
        "Accruing interest for {} seconds. Utilization rate: {}. Lending APR: {}. Borrowing APR: {}. Group fee APR: {}. Insurance fee APR: {}",
        time_delta,
        utilization_rate,
        lending_apr,
        borrowing_apr,
        group_fee_apr,
        insurance_fee_apr);

    Some((
        calc_accrued_interest_payment_per_period(lending_apr, time_delta, deposit_share_value)?,
        calc_accrued_interest_payment_per_period(borrowing_apr, time_delta, liability_share_value)?,
        calc_interest_payment_for_period(group_fee_apr, time_delta, total_liabilities)?,
        calc_interest_payment_for_period(insurance_fee_apr, time_delta, total_liabilities)?,
    ))
}

/// Calculates the fee rate for a given base rate and fees specified.
/// The returned rate is only the fee rate without the base rate.
///
/// Used for calculating the fees charged to the borrowers.
fn calc_fee_rate(base_rate: I80F48, rate_fees: I80F48, fixed_fees: I80F48) -> Option<I80F48> {
    base_rate.checked_mul(rate_fees)?.checked_add(fixed_fees)
}

/// Calculates the accrued interest payment per period `time_delta` in a principal value `value` for interest rate (in APR) `arp`.
/// Result is the new principal value.
fn calc_accrued_interest_payment_per_period(
    apr: I80F48,
    time_delta: u64,
    value: I80F48,
) -> Option<I80F48> {
    let ir_per_second = apr.checked_div(SECONDS_PER_YEAR)?;
    let new_value = value
        .checked_mul(I80F48::ONE.checked_add(ir_per_second.checked_mul(time_delta.into())?)?)?;

    Some(new_value)
}

/// Calculates the interest payment for a given period `time_delta` in a principal value `value` for interest rate (in APR) `arp`.
/// Result is the interest payment.
fn calc_interest_payment_for_period(apr: I80F48, time_delta: u64, value: I80F48) -> Option<I80F48> {
    let ir_per_second = apr.checked_div(SECONDS_PER_YEAR)?;
    let interest_payment = value
        .checked_mul(ir_per_second)?
        .checked_mul(time_delta.into())?;
    Some(interest_payment)
}

#[cfg_attr(
    any(feature = "test", feature = "client"),
    derive(Debug, PartialEq, Eq)
)]
#[zero_copy]
#[derive(AnchorDeserialize, AnchorSerialize)]
/// TODO: Convert weights to (u64, u64) to avoid precision loss (maybe?)
pub struct BankConfig {
    pub deposit_weight_init: WrappedI80F48,
    pub deposit_weight_maint: WrappedI80F48,

    pub liability_weight_init: WrappedI80F48,
    pub liability_weight_maint: WrappedI80F48,

    pub max_capacity: u64,

    pub pyth_oracle: Pubkey,
    pub interest_rate_config: InterestRateConfig,
}

impl Default for BankConfig {
    fn default() -> Self {
        Self {
            deposit_weight_init: I80F48::ZERO.into(),
            deposit_weight_maint: I80F48::ZERO.into(),
            liability_weight_init: I80F48::ONE.into(),
            liability_weight_maint: I80F48::ONE.into(),
            max_capacity: 0,
            pyth_oracle: Default::default(),
            interest_rate_config: Default::default(),
        }
    }
}

impl BankConfig {
    pub fn get_weights(&self, weight_type: WeightType) -> (I80F48, I80F48) {
        match weight_type {
            WeightType::Initial => (
                self.deposit_weight_init.into(),
                self.liability_weight_init.into(),
            ),
            WeightType::Maintenance => (
                self.deposit_weight_maint.into(),
                self.liability_weight_maint.into(),
            ),
        }
    }
}

#[zero_copy]
#[cfg_attr(any(feature = "test", feature = "client"), derive(PartialEq, Eq))]
#[derive(Debug, Default, AnchorDeserialize, AnchorSerialize)]
pub struct WrappedI80F48 {
    pub value: i128,
}

impl From<I80F48> for WrappedI80F48 {
    fn from(i: I80F48) -> Self {
        Self { value: i.to_bits() }
    }
}

impl From<WrappedI80F48> for I80F48 {
    fn from(w: WrappedI80F48) -> Self {
        Self::from_bits(w.value)
    }
}

#[derive(AnchorDeserialize, AnchorSerialize)]
pub struct BankConfigOpt {
    pub deposit_weight_init: Option<WrappedI80F48>,
    pub deposit_weight_maint: Option<WrappedI80F48>,

    pub liability_weight_init: Option<WrappedI80F48>,
    pub liability_weight_maint: Option<WrappedI80F48>,

    pub max_capacity: Option<u64>,

    pub pyth_oracle: Option<Pubkey>,
}

#[derive(Debug, Clone)]
pub enum BankVaultType {
    Liquidity,
    Insurance,
    Fee,
}

impl BankVaultType {
    pub fn get_seed(self) -> &'static [u8] {
        match self {
            BankVaultType::Liquidity => LIQUIDITY_VAULT_SEED,
            BankVaultType::Insurance => INSURANCE_VAULT_SEED,
            BankVaultType::Fee => FEE_VAULT_SEED,
        }
    }

    pub fn get_authority_seed(self) -> &'static [u8] {
        match self {
            BankVaultType::Liquidity => LIQUIDITY_VAULT_AUTHORITY_SEED,
            BankVaultType::Insurance => INSURANCE_VAULT_AUTHORITY_SEED,
            BankVaultType::Fee => FEE_VAULT_AUTHORITY_SEED,
        }
    }
}

#[macro_export]
macro_rules! assert_eq_with_tolerance {
    ($test_val:expr, $val:expr, $tolerance:expr) => {
        assert!(
            ($test_val - $val).abs() <= $tolerance,
            "assertion failed: `({} - {}) <= {}`",
            $test_val,
            $val,
            $tolerance
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixed_macro::types::I80F48;

    #[test]
    /// Tests that the interest payment for a 1 year period with 100% APR is 1.
    fn interest_payment_100apr_1year() {
        let apr = I80F48::ONE;
        let time_delta = 31_536_000; // 1 year
        let value = I80F48::ONE;

        assert_eq_with_tolerance!(
            calc_interest_payment_for_period(apr, time_delta, value).unwrap(),
            I80F48::ONE,
            I80F48!(0.001)
        );
    }

    /// Tests that the interest payment for a 1 year period with 50% APR is 0.5.
    #[test]
    fn interest_payment_50apr_1year() {
        let apr = I80F48::from_num(0.5);
        let time_delta = 31_536_000; // 1 year
        let value = I80F48::ONE;

        assert_eq_with_tolerance!(
            calc_interest_payment_for_period(apr, time_delta, value).unwrap(),
            I80F48::from_num(0.5),
            I80F48!(0.001)
        );
    }
    /// P: 1_000_000
    /// Apr: 12%
    /// Time: 1 second
    #[test]
    fn interest_payment_12apr_1second() {
        let apr = I80F48!(0.12);
        let time_delta = 1;
        let value = I80F48!(1_000_000);

        assert_eq_with_tolerance!(
            calc_interest_payment_for_period(apr, time_delta, value).unwrap(),
            I80F48!(0.0038),
            I80F48!(0.001)
        );
    }

    #[test]
    /// apr: 100%
    /// time: 1 year
    /// principal: 2
    /// expected: 4
    fn accrued_interest_apr100_year1() {
        assert_eq_with_tolerance!(
            calc_accrued_interest_payment_per_period(I80F48!(1), 31_536_000, I80F48!(2)).unwrap(),
            I80F48!(4),
            I80F48!(0.001)
        );
    }

    #[test]
    /// apr: 50%
    /// time: 1 year
    /// principal: 2
    /// expected: 3
    fn accrued_interest_apr50_year1() {
        assert_eq_with_tolerance!(
            calc_accrued_interest_payment_per_period(I80F48!(0.5), 31_536_000, I80F48!(2)).unwrap(),
            I80F48!(3),
            I80F48!(0.001)
        );
    }

    #[test]
    /// apr: 12%
    /// time: 1 second
    /// principal: 1_000_000
    /// expected: 1_038
    fn accrued_interest_apr12_year1() {
        assert_eq_with_tolerance!(
            calc_accrued_interest_payment_per_period(I80F48!(0.12), 1, I80F48!(1_000_000)).unwrap(),
            I80F48!(1_000_000.0038),
            I80F48!(0.001)
        );
    }

    #[test]
    /// ur: 0
    /// protocol_fixed_fee: 0.01
    fn ir_config_calc_interest_rate_pff_01() {
        let config = InterestRateConfig {
            optimal_utilization_rate: I80F48!(0.6).into(),
            plateau_interest_rate: I80F48!(0.40).into(),
            protocol_fixed_fee_apr: I80F48!(0.01).into(),
            ..Default::default()
        };

        let (lending_apr, borrow_apr, group_fees_apr, insurance_apr) =
            config.calc_interest_rate(I80F48!(0)).unwrap();

        assert_eq_with_tolerance!(lending_apr, I80F48!(0), I80F48!(0.001));
        assert_eq_with_tolerance!(borrow_apr, I80F48!(0.01), I80F48!(0.001));
        assert_eq_with_tolerance!(group_fees_apr, I80F48!(0.01), I80F48!(0.001));
        assert_eq_with_tolerance!(insurance_apr, I80F48!(0), I80F48!(0.001));
    }

    #[test]
    /// ur: 0.5
    /// protocol_fixed_fee: 0.01
    /// optimal_utilization_rate: 0.5
    /// plateau_interest_rate: 0.4
    fn ir_config_calc_interest_rate_pff_01_ur_05() {
        let config = InterestRateConfig {
            optimal_utilization_rate: I80F48!(0.5).into(),
            plateau_interest_rate: I80F48!(0.4).into(),
            protocol_fixed_fee_apr: I80F48!(0.01).into(),
            insurance_ir_fee: I80F48!(0.1).into(),
            ..Default::default()
        };

        let (lending_apr, borrow_apr, group_fees_apr, insurance_apr) =
            config.calc_interest_rate(I80F48!(0.5)).unwrap();

        assert_eq_with_tolerance!(lending_apr, I80F48!(0.2), I80F48!(0.001));
        assert_eq_with_tolerance!(borrow_apr, I80F48!(0.45), I80F48!(0.001));
        assert_eq_with_tolerance!(group_fees_apr, I80F48!(0.01), I80F48!(0.001));
        assert_eq_with_tolerance!(insurance_apr, I80F48!(0.04), I80F48!(0.001));
    }

    /// ur: 0.8
    /// protocol_fixed_fee: 0.01
    /// optimal_utilization_rate: 0.5
    /// plateau_interest_rate: 0.4
    /// max_interest_rate: 3
    /// insurance_ir_fee: 0.1
    #[test]
    fn ir_config_calc_interest_rate_pff_01_ur_08() {
        let config = InterestRateConfig {
            optimal_utilization_rate: I80F48!(0.4).into(),
            plateau_interest_rate: I80F48!(0.4).into(),
            protocol_fixed_fee_apr: I80F48!(0.01).into(),
            max_interest_rate: I80F48!(3).into(),
            insurance_ir_fee: I80F48!(0.1).into(),
            ..Default::default()
        };

        let (lending_apr, borrow_apr, group_fees_apr, insurance_apr) =
            config.calc_interest_rate(I80F48!(0.7)).unwrap();

        assert_eq_with_tolerance!(lending_apr, I80F48!(1.19), I80F48!(0.001));
        assert_eq_with_tolerance!(borrow_apr, I80F48!(1.88), I80F48!(0.001));
        assert_eq_with_tolerance!(group_fees_apr, I80F48!(0.01), I80F48!(0.001));
        assert_eq_with_tolerance!(insurance_apr, I80F48!(0.17), I80F48!(0.001));
    }
}
