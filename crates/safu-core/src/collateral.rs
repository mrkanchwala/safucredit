//! Collateral valuation for Token-2022 Scaled UI Amount tokens (xStocks).
//!
//! The raw token amount never changes; splits and dividends move the multiplier. Value is always
//! `raw × effective multiplier × price`, in integer math on raw amounts (spec 4).

use crate::{mul, pow10, wide::U256, CoreError, Result, MULT_SCALE, PRICE_DECIMALS, USD_DECIMALS};

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
/// `floor(raw × multiplier × price / (MULT_SCALE × 10^(PRICE_DECIMALS − USD_DECIMALS) × 10^decimals))`, rounded
/// down exactly once, so collateral is never overvalued. The product always fits in 256 bits (eng review E2),
/// so `Overflow` means only that the value itself exceeds `u64`.
pub fn collateral_value(raw: u64, decimals: u8, multiplier_fp: u128, price_fp: u64) -> Result<u64> {
    if multiplier_fp == 0 {
        return Err(CoreError::InvalidMultiplier);
    }
    // u64 × u64 < 2^128, and that × u128 < 2^256.
    let product = U256::full_mul(mul(raw as u128, price_fp as u128)?, multiplier_fp);
    product
        .div_rem(mul(MULT_SCALE, pow10(PRICE_DECIMALS - USD_DECIMALS)?)?)?
        .0
        .div_pow10(decimals as u32)?
        .to_u64()
}

/// Raw base units worth `usd` (6 decimals). Inverse of [`collateral_value`]:
/// `floor(usd × 10^(PRICE_DECIMALS − USD_DECIMALS) × 10^decimals × MULT_SCALE / (price × multiplier))`, rounded down
/// exactly once, so a liquidator never receives more collateral than they paid for. Computed in 256 bits;
/// `Overflow` when the numerator passes 2^256 − 1 (only at extreme `decimals`) or the result exceeds `u64`.
pub fn raw_for_usd(usd: u64, decimals: u8, multiplier_fp: u128, price_fp: u64) -> Result<u64> {
    if multiplier_fp == 0 {
        return Err(CoreError::InvalidMultiplier);
    }
    if price_fp == 0 {
        return Err(CoreError::DivideByZero);
    }
    let numerator = U256::full_mul(
        mul(usd as u128, pow10(PRICE_DECIMALS - USD_DECIMALS)?)?,
        MULT_SCALE,
    )
    .checked_mul_pow10(decimals as u32)?;
    numerator
        .div_rem(price_fp as u128)?
        .0
        .div_rem(multiplier_fp)?
        .0
        .to_u64()
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

    // Expected values below were computed independently with Python big integers, not with this crate.

    #[test]
    fn one_rounding_step_not_three() {
        // floor(123_456_789 × 1.0026642 × $330.28 / 10^22): the old three floor steps returned less.
        assert_eq!(
            collateral_value(123_456_789, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28),
            Ok(408_839_418)
        );
        assert_eq!(
            raw_for_usd(331_159_931, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28),
            Ok(99_999_999)
        );
    }

    #[test]
    fn u64_max_raw_and_price_value_exactly_instead_of_overflowing() {
        // (2^64 − 1)^2 × 1000.0 / 10^35: the old path overflowed u128 on the way.
        assert_eq!(
            collateral_value(u64::MAX, 21, 1_000 * MULT_SCALE, u64::MAX),
            Ok(3_402_823_669_209_384_634)
        );
        // The same inputs at 8 decimals are a real value past u64, and stay an error.
        assert_eq!(
            collateral_value(u64::MAX, 8, MULT_SCALE, u64::MAX),
            Err(CoreError::Overflow)
        );
    }

    #[test]
    fn raw_for_usd_divides_by_a_multiplier_wider_than_u64() {
        // 10^18 USD units at 18 decimals, $1.00, multiplier (10^23 + 7) / 10^12.
        assert_eq!(
            raw_for_usd(
                1_000_000_000_000_000_000,
                18,
                100_000_000_000_000_000_000_007,
                100_000_000
            ),
            Ok(9_999_999_999_999_999_999)
        );
        assert_eq!(
            raw_for_usd(u64::MAX, 255, MULT_SCALE, 1),
            Err(CoreError::Overflow)
        );
        assert_eq!(raw_for_usd(0, 255, MULT_SCALE, 1), Ok(0));
    }

    #[test]
    fn usd_to_raw_round_trip_never_exceeds_original() {
        let raw = 123_456_789;
        let v = collateral_value(raw, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28).unwrap();
        assert!(raw_for_usd(v, AAPLX_DECIMALS, AAPLX_MULT, PRICE_330_28).unwrap() <= raw);
    }
}
