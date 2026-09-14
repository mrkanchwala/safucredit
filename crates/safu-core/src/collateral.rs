//! Collateral valuation for Token-2022 Scaled UI Amount tokens (xStocks).
//!
//! The raw token amount never changes; splits and dividends move the multiplier. Value is always
//! `raw × effective multiplier × price`, in integer math on raw amounts (spec 4).

use crate::{div, mul, pow10, to_u64, CoreError, Result, MULT_SCALE, PRICE_DECIMALS, USD_DECIMALS};

/// Multiplier in effect at `now`. Token-2022's rule: the new value applies at `now >= effective_ts`, and the
/// stored `multiplier` field is NOT rewritten when that happens, so reading it alone goes stale.
pub fn effective_multiplier(
    now: i64,
    current_fp: u128,
    new_fp: u128,
    new_effective_ts: i64,
) -> u128 {
    if now >= new_effective_ts {
        new_fp
    } else {
        current_fp
    }
}

/// USD value (6 decimals) of `raw` base units of a token with `decimals`, at `multiplier_fp` and `price_fp`
/// (8 decimals, USD per whole share).
///
/// Three floor steps keep every intermediate inside `u128` for realistic inputs; each rounds down, so
/// collateral is never overvalued. Out-of-range inputs return `Overflow`.
pub fn collateral_value(raw: u64, decimals: u8, multiplier_fp: u128, price_fp: u64) -> Result<u64> {
    if multiplier_fp == 0 {
        return Err(CoreError::InvalidMultiplier);
    }
    let scaled_raw = div(mul(raw as u128, multiplier_fp)?, MULT_SCALE)?;
    let usd_price_units = div(mul(scaled_raw, price_fp as u128)?, pow10(decimals as u32)?)?;
    to_u64(div(usd_price_units, pow10(PRICE_DECIMALS - USD_DECIMALS)?)?)
}

/// Raw base units worth `usd` (6 decimals). Inverse of [`collateral_value`], rounded down, so a liquidator
/// never receives more collateral than they paid for.
pub fn raw_for_usd(usd: u64, decimals: u8, multiplier_fp: u128, price_fp: u64) -> Result<u64> {
    if multiplier_fp == 0 {
        return Err(CoreError::InvalidMultiplier);
    }
    let usd_price_units = mul(
        mul(usd as u128, pow10(PRICE_DECIMALS - USD_DECIMALS)?)?,
        pow10(decimals as u32)?,
    )?;
    let scaled_raw = div(usd_price_units, price_fp as u128)?;
    to_u64(div(mul(scaled_raw, MULT_SCALE)?, multiplier_fp)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AAPLx as read on mainnet 2026-09-13/14: 8 decimals, multiplier 1.0026642, price ~$330.28.
    const AAPLX_DECIMALS: u8 = 8;
    const AAPLX_MULT: u128 = 1_002_664_200_000;
    const PRICE_330_28: u64 = 33_028_000_000;

    #[test]
    fn one_aaplx_share_values_at_price_times_multiplier() {
        // 330.28 × 1.0026642 = 331.15993..., rounded down to 6 decimals.
        let v = collateral_value(100_000_000, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28).unwrap();
        assert_eq!(v, 331_159_931);
    }

    #[test]
    fn four_for_one_split_keeps_value_when_price_and_multiplier_move_together() {
        let before =
            collateral_value(100_000_000, AAPLX_DECIMALS, MULT_SCALE, 40_000_000_000).unwrap();
        let after =
            collateral_value(100_000_000, AAPLX_DECIMALS, 4 * MULT_SCALE, 10_000_000_000).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn desync_cases_move_value_by_the_split_ratio() {
        let fair =
            collateral_value(100_000_000, AAPLX_DECIMALS, MULT_SCALE, 40_000_000_000).unwrap();
        // Price updated first (÷4), multiplier not yet: collateral reads 4× too low → wrongful liquidation.
        let price_first =
            collateral_value(100_000_000, AAPLX_DECIMALS, MULT_SCALE, 10_000_000_000).unwrap();
        // Multiplier first (×4), price not yet: collateral reads 4× too high → over-borrowing.
        let mult_first =
            collateral_value(100_000_000, AAPLX_DECIMALS, 4 * MULT_SCALE, 40_000_000_000).unwrap();
        assert_eq!(price_first * 4, fair);
        assert_eq!(mult_first, fair * 4);
    }

    #[test]
    fn effective_multiplier_switches_exactly_at_timestamp() {
        assert_eq!(effective_multiplier(99, 1, 4, 100), 1);
        assert_eq!(effective_multiplier(100, 1, 4, 100), 4);
        assert_eq!(effective_multiplier(101, 1, 4, 100), 4);
    }

    #[test]
    fn zero_multiplier_and_zero_price_are_errors_not_panics() {
        assert_eq!(
            collateral_value(1, 8, 0, 1),
            Err(CoreError::InvalidMultiplier)
        );
        assert_eq!(
            raw_for_usd(1, 8, MULT_SCALE, 0),
            Err(CoreError::DivideByZero)
        );
    }

    #[test]
    fn usd_to_raw_round_trip_never_exceeds_original() {
        let raw = 123_456_789;
        let v = collateral_value(raw, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28).unwrap();
        assert!(raw_for_usd(v, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28).unwrap() <= raw);
    }
}
