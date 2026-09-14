//! Borrowing limits, liquidation trigger, interest (kinked utilization curve, A5 §5), debt shares.

use crate::{add, apply_bps, collateral::raw_for_usd, div, div_ceil, mul, to_u64, CoreError, Result, BPS};

/// Debt index fixed-point scale: 1.0 == 1_000_000_000_000.
pub const INDEX_SCALE: u128 = 1_000_000_000_000;
pub const SECONDS_PER_YEAR: u128 = 31_536_000;

/// Largest debt allowed against `collateral_value` at `ltv_bps`.
pub fn max_borrow(collateral_value: u64, ltv_bps: u32) -> Result<u64> {
    apply_bps(collateral_value, ltv_bps)
}

/// Liquidatable when `debt × 10_000 > collateral_value × liq_threshold_bps`.
pub fn is_liquidatable(debt: u64, collateral_value: u64, liq_threshold_bps: u32) -> Result<bool> {
    Ok(mul(debt as u128, BPS)? > mul(collateral_value as u128, liq_threshold_bps as u128)?)
}

/// Share of supplied USDC currently lent out, in bps.
pub fn utilization_bps(borrowed: u64, cash: u64) -> Result<u32> {
    let total = add(borrowed as u128, cash as u128)?;
    if total == 0 {
        return Ok(0);
    }
    Ok(div(mul(borrowed as u128, BPS)?, total)? as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateCurve {
    pub base_bps: u32,
    pub slope1_bps: u32,
    pub slope2_bps: u32,
    /// Utilization where the steep slope starts. Must be in (0, 10_000).
    pub kink_bps: u32,
}

/// Annual borrow rate in bps for a given utilization.
pub fn borrow_rate_bps(utilization: u32, curve: RateCurve) -> Result<u32> {
    let kink = curve.kink_bps as u128;
    if kink == 0 || kink >= BPS || utilization as u128 > BPS {
        return Err(CoreError::InvalidParameter);
    }
    let u = utilization as u128;
    let rate = if u <= kink {
        add(curve.base_bps as u128, div(mul(curve.slope1_bps as u128, u)?, kink)?)?
    } else {
        let steep = div(mul(curve.slope2_bps as u128, u - kink)?, BPS - kink)?;
        add(add(curve.base_bps as u128, curve.slope1_bps as u128)?, steep)?
    };
    u32::try_from(rate).map_err(|_| CoreError::Overflow)
}

/// Grows the debt index by simple interest over `elapsed_secs`. Rounds up (debt never under-accrues).
pub fn accrue_index(index: u128, rate_bps: u32, elapsed_secs: u64) -> Result<u128> {
    let growth = div_ceil(
        mul(mul(index, rate_bps as u128)?, elapsed_secs as u128)?,
        mul(BPS, SECONDS_PER_YEAR)?,
    )?;
    add(index, growth)
}

/// Debt represented by `shares` at `index`. Rounds up.
pub fn debt_for_shares(shares: u128, index: u128) -> Result<u64> {
    to_u64(div_ceil(mul(shares, index)?, INDEX_SCALE)?)
}

/// Shares minted when borrowing `amount`. Rounds up (the borrower owes at least what they took).
pub fn shares_for_borrow(amount: u64, index: u128) -> Result<u128> {
    div_ceil(mul(amount as u128, INDEX_SCALE)?, index)
}

/// Shares burned when repaying `amount`. Rounds down (a repayment never cancels more than it pays).
pub fn shares_for_repay(amount: u64, index: u128) -> Result<u128> {
    div(mul(amount as u128, INDEX_SCALE)?, index)
}

/// Collateral a liquidator receives for repaying `repay_usd`, including `bonus_bps`. Rounds down.
pub fn seize_for_repay(
    repay_usd: u64,
    bonus_bps: u32,
    decimals: u8,
    multiplier_fp: u128,
    price_fp: u64,
) -> Result<u64> {
    let with_bonus = apply_bps(repay_usd, (BPS as u32).checked_add(bonus_bps).ok_or(CoreError::Overflow)?)?;
    raw_for_usd(with_bonus, decimals, multiplier_fp, price_fp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE: RateCurve = RateCurve { base_bps: 200, slope1_bps: 800, slope2_bps: 6_000, kink_bps: 8_000 };

    #[test]
    fn liquidation_threshold_boundary() {
        // debt 75, value 100, threshold 75% → exactly at threshold is NOT liquidatable.
        assert_eq!(is_liquidatable(75, 100, 7_500), Ok(false));
        assert_eq!(is_liquidatable(76, 100, 7_500), Ok(true));
    }

    #[test]
    fn rate_curve_kink() {
        assert_eq!(borrow_rate_bps(0, CURVE), Ok(200));
        assert_eq!(borrow_rate_bps(8_000, CURVE), Ok(1_000));
        assert_eq!(borrow_rate_bps(9_000, CURVE), Ok(4_000));
        assert_eq!(borrow_rate_bps(10_000, CURVE), Ok(7_000));
        assert_eq!(borrow_rate_bps(10_001, CURVE), Err(CoreError::InvalidParameter));
    }

    #[test]
    fn utilization_empty_market_is_zero() {
        assert_eq!(utilization_bps(0, 0), Ok(0));
        assert_eq!(utilization_bps(50, 50), Ok(5_000));
    }

    #[test]
    fn one_year_at_10_percent_grows_index_by_10_percent() {
        let idx = accrue_index(INDEX_SCALE, 1_000, SECONDS_PER_YEAR as u64).unwrap();
        assert_eq!(idx, INDEX_SCALE + INDEX_SCALE / 10);
    }

    #[test]
    fn share_rounding_favours_the_protocol() {
        let index = INDEX_SCALE + 1; // any index that is not exactly 1.0
        let shares = shares_for_borrow(1_000_001, index).unwrap();
        assert!(debt_for_shares(shares, index).unwrap() >= 1_000_001);
        assert!(shares_for_repay(1_000_001, index).unwrap() <= shares);
    }
}
