//! Price-feed guards (spec 5): TWAP, single-update deviation cap, off-hours band, split pause window.

use crate::{add, bps_diff, div, mul, to_u64, CoreError, Result, BPS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// USD per whole share, 8 decimals.
    pub price_fp: u64,
    /// Unix seconds.
    pub ts: i64,
}

/// Time-weighted average price. Each sample holds from its own timestamp until the next sample's; the last
/// holds until `now`. Samples must be ascending by timestamp and none may be after `now`.
pub fn twap(samples: &[Sample], now: i64) -> Result<u64> {
    let last = samples.last().ok_or(CoreError::EmptySamples)?;
    let mut weighted: u128 = 0;
    let mut total: u128 = 0;
    for (i, s) in samples.iter().enumerate() {
        let end = samples.get(i + 1).map_or(now, |next| next.ts);
        let dt = end.checked_sub(s.ts).ok_or(CoreError::Overflow)?;
        if dt < 0 {
            return Err(CoreError::UnsortedSamples);
        }
        weighted = add(weighted, mul(s.price_fp as u128, dt as u128)?)?;
        total = add(total, dt as u128)?;
    }
    if total == 0 {
        return Ok(last.price_fp);
    }
    to_u64(div(weighted, total)?)
}

/// True when `new_price` moves more than `cap_bps` away from `reference` (normally the TWAP). A flagged update
/// must not enter the TWAP and must never trigger a liquidation.
pub fn deviation_exceeded(new_price: u64, reference: u64, cap_bps: u32) -> Result<bool> {
    Ok(bps_diff(new_price, reference)? > cap_bps as u128)
}

/// Off-hours guard: clamps `price` into `last_close ± band_bps`. Callers apply it only while the underlying
/// market is closed.
pub fn clamp_to_band(price: u64, last_close: u64, band_bps: u32) -> Result<u64> {
    let band = band_bps as u128;
    if band > BPS {
        return Err(CoreError::InvalidParameter);
    }
    let lo = to_u64(div(mul(last_close as u128, BPS - band)?, BPS)?)?;
    let hi = to_u64(div(mul(last_close as u128, BPS + band)?, BPS)?)?;
    Ok(price.clamp(lo, hi))
}

/// True while liquidations must pause around a scheduled multiplier change: the change is larger than `cap_bps`
/// (a split or reverse split, not a routine dividend step) and `now` is within `window_secs` of the effective
/// timestamp, on either side.
pub fn split_window_active(
    now: i64,
    current_fp: u128,
    new_fp: u128,
    new_effective_ts: i64,
    cap_bps: u32,
    window_secs: i64,
) -> Result<bool> {
    if current_fp == 0 || new_fp == 0 {
        return Err(CoreError::InvalidMultiplier);
    }
    if window_secs < 0 {
        return Err(CoreError::InvalidParameter);
    }
    let change_bps = div(mul(current_fp.abs_diff(new_fp), BPS)?, current_fp)?;
    if change_bps <= cap_bps as u128 {
        return Ok(false);
    }
    let start = new_effective_ts.saturating_sub(window_secs);
    let end = new_effective_ts.saturating_add(window_secs);
    Ok(now >= start && now <= end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MULT_SCALE;

    fn s(price_fp: u64, ts: i64) -> Sample {
        Sample { price_fp, ts }
    }

    #[test]
    fn twap_weights_by_time_held() {
        // 100 held 30s, 200 held 10s → (3000 + 2000) / 40 = 125.
        assert_eq!(twap(&[s(100, 0), s(200, 30)], 40), Ok(125));
    }

    #[test]
    fn twap_errors() {
        assert_eq!(twap(&[], 10), Err(CoreError::EmptySamples));
        assert_eq!(
            twap(&[s(1, 20), s(2, 10)], 30),
            Err(CoreError::UnsortedSamples)
        );
        assert_eq!(twap(&[s(1, 20)], 10), Err(CoreError::UnsortedSamples));
    }

    #[test]
    fn twap_zero_duration_returns_last() {
        assert_eq!(twap(&[s(7, 5), s(9, 5)], 5), Ok(9));
    }

    #[test]
    fn deviation_cap_boundary_is_inclusive_of_the_cap() {
        assert_eq!(deviation_exceeded(10_500, 10_000, 500), Ok(false));
        assert_eq!(deviation_exceeded(10_501, 10_000, 500), Ok(true));
        assert_eq!(deviation_exceeded(9_499, 10_000, 500), Ok(true));
    }

    #[test]
    fn off_hours_band_clamps_both_sides() {
        assert_eq!(clamp_to_band(12_000, 10_000, 1_000), Ok(11_000));
        assert_eq!(clamp_to_band(8_000, 10_000, 1_000), Ok(9_000));
        assert_eq!(clamp_to_band(10_300, 10_000, 1_000), Ok(10_300));
        assert_eq!(
            clamp_to_band(1, 10_000, 10_001),
            Err(CoreError::InvalidParameter)
        );
    }

    #[test]
    fn dividend_step_never_pauses() {
        // Live AAPLx step 1.0026642 → 1.0032690 (~6 bps) against a 500 bps cap.
        let active = split_window_active(
            1_000,
            1_002_664_200_000,
            1_003_269_000_000,
            1_000,
            500,
            3_600,
        )
        .unwrap();
        assert!(!active);
    }

    #[test]
    fn split_pauses_inside_window_edges_only() {
        let ts = 1_000_000;
        let w = 3_600;
        let check = |now| split_window_active(now, MULT_SCALE, 4 * MULT_SCALE, ts, 500, w).unwrap();
        assert!(!check(ts - w - 1));
        assert!(check(ts - w));
        assert!(check(ts));
        assert!(check(ts + w));
        assert!(!check(ts + w + 1));
    }

    #[test]
    fn reverse_split_also_pauses() {
        assert!(split_window_active(0, 4 * MULT_SCALE, MULT_SCALE, 0, 500, 60).unwrap());
    }
}
