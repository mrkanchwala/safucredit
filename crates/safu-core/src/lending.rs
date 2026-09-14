//! Borrowing limits, liquidation trigger, interest (kinked utilization curve, A5 §5), debt shares.

use crate::{
    add, apply_bps, collateral::raw_for_usd, div, div_ceil, mul, to_u64, CoreError, Result, BPS,
};

/// Debt index fixed-point scale: 1.0 == 1_000_000_000_000.
pub const INDEX_SCALE: u128 = 1_000_000_000_000;
pub const SECONDS_PER_YEAR: u128 = 31_536_000;

/// A basis-point term moving linearly from `from` to `to` over `duration_secs` starting at `start` (U3).
///
/// Before `start` it is `from`; at or after `start + duration_secs` it is `to`. In between, the step is
/// truncated toward `from`, so a ramp is never ahead of schedule. `duration_secs <= 0` means no ramp.
/// Pure and panic-free: time arithmetic saturates.
pub fn ramp_bps(from: u32, to: u32, start: i64, now: i64, duration_secs: i64) -> u32 {
    if duration_secs <= 0 {
        return to;
    }
    // Checked before the end: with `start` near i64::MAX the end saturates onto `start`, and a ramp
    // must still read `from` at its own start rather than counting as already finished.
    if now <= start {
        return from;
    }
    if now >= start.saturating_add(duration_secs) {
        return to;
    }
    let elapsed = (now - start) as i128;
    let delta = to as i128 - from as i128;
    // |delta| < 2^32 and elapsed < duration < 2^63, so the product fits in i128.
    (from as i128 + delta * elapsed / duration_secs as i128) as u32
}

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
        add(
            curve.base_bps as u128,
            div(mul(curve.slope1_bps as u128, u)?, kink)?,
        )?
    } else {
        let steep = div(mul(curve.slope2_bps as u128, u - kink)?, BPS - kink)?;
        add(
            add(curve.base_bps as u128, curve.slope1_bps as u128)?,
            steep,
        )?
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

/// Position LTV in basis points: `debt × 10_000 / collateral_value`.
///
/// Collateral valued at zero with debt outstanding is maximally unhealthy rather than an error —
/// a liquidation must still be possible there, and the bonus and repay caps below both read this.
pub fn current_ltv_bps(debt: u64, collateral_value: u64) -> Result<u32> {
    if collateral_value == 0 {
        return Ok(if debt == 0 { 0 } else { u32::MAX });
    }
    let ltv = div(mul(debt as u128, BPS)?, collateral_value as u128)?;
    Ok(u32::try_from(ltv).unwrap_or(u32::MAX))
}

/// Dynamic liquidation bonus (market config 2026-09-14, matched to the market leader's shape).
///
/// It grows with how far past the liquidation line a position has drifted, floored at `min_bps` so
/// a liquidator is always paid to act, capped at `max_bps`, and capped again by **solvency**:
/// `10_000 − current_ltv`. That last cap is what keeps `repay × (1 + bonus)` inside the collateral
/// that actually exists. Past 100% LTV it returns 0 — there is no room for a bonus, and the
/// shortfall becomes bad debt instead of being taken out of someone else's collateral.
pub fn liquidation_bonus_bps(
    current_ltv_bps: u32,
    liq_threshold_bps: u32,
    min_bps: u32,
    max_bps: u32,
) -> Result<u32> {
    if min_bps > max_bps {
        return Err(CoreError::InvalidParameter);
    }
    let drift = current_ltv_bps.saturating_sub(liq_threshold_bps);
    let wanted = drift.max(min_bps).min(max_bps);
    let solvency = (BPS as u32).saturating_sub(current_ltv_bps);
    Ok(wanted.min(solvency))
}

/// Largest debt a single liquidation may repay.
///
/// Three bounds: the close factor's share of the debt; the whole debt once the position is past
/// `insolvency_ltv_bps` (below that line, chipping away is enough); and never more than
/// `max_liquidation_debt`, which is sized from *measured* collateral liquidity so one liquidation
/// cannot move the market against itself. The Binance 10-11 Oct 2025 cascade is the shape this
/// last bound exists to avoid.
pub fn max_repay(
    debt: u64,
    current_ltv_bps: u32,
    insolvency_ltv_bps: u32,
    close_factor_bps: u32,
    max_liquidation_debt: u64,
) -> Result<u64> {
    let allowed = if current_ltv_bps > insolvency_ltv_bps {
        debt
    } else {
        apply_bps(debt, close_factor_bps)?
    };
    Ok(allowed.min(max_liquidation_debt))
}

/// Collateral a liquidator receives for repaying `repay_usd`, including `bonus_bps`. Rounds down.
pub fn seize_for_repay(
    repay_usd: u64,
    bonus_bps: u32,
    decimals: u8,
    multiplier_fp: u128,
    price_fp: u64,
) -> Result<u64> {
    let with_bonus = apply_bps(
        repay_usd,
        (BPS as u32)
            .checked_add(bonus_bps)
            .ok_or(CoreError::Overflow)?,
    )?;
    raw_for_usd(with_bonus, decimals, multiplier_fp, price_fp)
}

/// Debt a liquidator actually repays for `seized_value` of collateral at `bonus_bps` — the inverse
/// of [`seize_for_repay`]'s bonus step.
///
/// Needed when a position holds less collateral than the requested repayment would seize. Without
/// it the liquidator pays the full amount and receives whatever is left, and because the repayment
/// then clears the whole debt, the shortfall is never recognised — suppliers take the loss with no
/// record of what the backstop owes them. Rounds up, so the protocol never under-charges.
pub fn repay_for_seized(seized_value: u64, bonus_bps: u32) -> Result<u64> {
    to_u64(div_ceil(
        mul(seized_value as u128, BPS)?,
        add(BPS, bonus_bps as u128)?,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE: RateCurve = RateCurve {
        base_bps: 200,
        slope1_bps: 800,
        slope2_bps: 6_000,
        kink_bps: 8_000,
    };

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
        assert_eq!(
            borrow_rate_bps(10_001, CURVE),
            Err(CoreError::InvalidParameter)
        );
    }

    #[test]
    fn bonus_grows_with_drift_then_stops_at_the_cap() {
        // threshold 5_000, bonus band 100..500 bps.
        let b = |ltv| liquidation_bonus_bps(ltv, 5_000, 100, 500).unwrap();
        assert_eq!(b(5_001), 100, "just past the line pays the floor");
        assert_eq!(b(5_300), 300, "drift of 300 bps pays 300");
        assert_eq!(b(5_500), 500, "drift of 500 bps pays the ceiling");
        assert_eq!(b(6_000), 500, "and never more");
    }

    #[test]
    fn bonus_is_capped_by_solvency_and_vanishes_past_full_ltv() {
        // At 9_800 LTV only 200 bps of collateral is left over the debt, so that is the cap.
        assert_eq!(liquidation_bonus_bps(9_800, 5_000, 100, 500), Ok(200));
        assert_eq!(liquidation_bonus_bps(10_000, 5_000, 100, 500), Ok(0));
        assert_eq!(liquidation_bonus_bps(12_000, 5_000, 100, 500), Ok(0));
        assert_eq!(
            liquidation_bonus_bps(6_000, 5_000, 600, 500),
            Err(CoreError::InvalidParameter)
        );
    }

    #[test]
    fn ltv_handles_a_worthless_position_without_dividing_by_zero() {
        assert_eq!(current_ltv_bps(0, 0), Ok(0));
        assert_eq!(current_ltv_bps(1, 0), Ok(u32::MAX));
        assert_eq!(current_ltv_bps(50, 100), Ok(5_000));
        assert_eq!(current_ltv_bps(150, 100), Ok(15_000));
    }

    #[test]
    fn repay_is_bounded_by_close_factor_insolvency_and_the_chunk_cap() {
        // Below the insolvency line: 25% of a 1_000 debt.
        assert_eq!(max_repay(1_000, 6_000, 9_500, 2_500, u64::MAX), Ok(250));
        // Past it: the whole debt in one go.
        assert_eq!(max_repay(1_000, 9_501, 9_500, 2_500, u64::MAX), Ok(1_000));
        // The chunk cap wins over both.
        assert_eq!(max_repay(1_000, 9_501, 9_500, 2_500, 400), Ok(400));
        // Exactly at the insolvency line is NOT past it.
        assert_eq!(max_repay(1_000, 9_500, 9_500, 2_500, u64::MAX), Ok(250));
    }

    #[test]
    fn a_liquidator_never_receives_more_collateral_than_they_paid_for() {
        // Bonus applied, then converted back: the seized value must not exceed repay × (1 + bonus).
        let seized = seize_for_repay(1_000_000, 500, 8, crate::MULT_SCALE, 10_000_000_000).unwrap();
        let back =
            crate::collateral::collateral_value(seized, 8, crate::MULT_SCALE, 10_000_000_000)
                .unwrap();
        assert!(back <= 1_050_000, "got {back}");
    }

    #[test]
    fn repay_for_seized_inverts_the_bonus_without_undercharging() {
        // $105 of collateral at a 5% bonus was bought with $100 of repayment.
        assert_eq!(repay_for_seized(105_000_000, 500), Ok(100_000_000));
        assert_eq!(repay_for_seized(100, 0), Ok(100));
        // Round trip never lets the liquidator get collateral they did not pay for.
        for value in [1u64, 7, 999, 1_000_000, u64::MAX / 20_000] {
            for bonus in [0u32, 1, 100, 500, 2_000] {
                let repay = repay_for_seized(value, bonus).unwrap();
                let back = apply_bps(repay, 10_000 + bonus).unwrap();
                assert!(
                    back >= value,
                    "value {value} bonus {bonus}: {back} < {value}"
                );
            }
        }
    }

    #[test]
    fn ramp_moves_linearly_and_never_runs_ahead() {
        let week = 7 * 86_400;
        // 50% → 42% over a week.
        assert_eq!(
            ramp_bps(5_000, 4_200, 100, 99, week),
            5_000,
            "before the start"
        );
        assert_eq!(
            ramp_bps(5_000, 4_200, 100, 100, week),
            5_000,
            "at the start"
        );
        assert_eq!(
            ramp_bps(5_000, 4_200, 100, 100 + week / 2, week),
            4_600,
            "half way"
        );
        assert_eq!(ramp_bps(5_000, 4_200, 100, 100 + week, week), 4_200, "done");
        assert_eq!(ramp_bps(5_000, 4_200, 100, i64::MAX, week), 4_200);
        // One second in, the 0.0013 bps step truncates toward `from`: never ahead of schedule.
        assert_eq!(ramp_bps(5_000, 4_200, 100, 101, week), 5_000);
        // Upward ramps (a rising bonus) truncate toward `from` too.
        assert_eq!(ramp_bps(100, 500, 0, 1, week), 100);
        assert_eq!(ramp_bps(100, 500, 0, week / 4, week), 200);
        // No ramp.
        assert_eq!(ramp_bps(5_000, 4_200, 100, 100, 0), 4_200);
        assert_eq!(ramp_bps(7, 7, 0, 5, week), 7);
        // Saturating time arithmetic at the extremes.
        assert_eq!(ramp_bps(1, u32::MAX, i64::MAX, i64::MAX, week), 1);
        assert_eq!(
            ramp_bps(u32::MAX, 0, i64::MIN, i64::MIN + 1, i64::MAX),
            u32::MAX
        );
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
