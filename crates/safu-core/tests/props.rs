//! Property tests: safu-core must never panic on any input, and its rounding must always favour the protocol.

use proptest::prelude::*;
use safu_core::{
    collateral::{collateral_value, effective_multiplier, raw_for_usd},
    lending::{
        debt_for_shares, is_liquidatable, max_borrow, seize_for_repay, shares_for_borrow,
        shares_for_repay,
    },
    loss::{payout, wrongful_loss},
    price::{clamp_to_band, split_window_active, twap, Sample},
    MULT_SCALE,
};

fn multiplier() -> impl Strategy<Value = u128> {
    // 0.001 to 1000.0
    (MULT_SCALE / 1_000)..=(MULT_SCALE * 1_000)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn valuation_never_panics(raw in any::<u64>(), d in 0u8..=40, m in any::<u128>(), p in any::<u64>()) {
        let _ = collateral_value(raw, d, m, p);
        let _ = raw_for_usd(raw, d, m, p);
    }

    #[test]
    fn valuation_is_monotonic_in_raw(a in any::<u64>(), b in any::<u64>(), d in 0u8..=18, m in multiplier(), p in 1u64..=10_000_000_000_000) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        if let (Ok(v_lo), Ok(v_hi)) = (collateral_value(lo, d, m, p), collateral_value(hi, d, m, p)) {
            prop_assert!(v_lo <= v_hi);
        }
    }

    #[test]
    fn valuation_is_monotonic_in_price(raw in any::<u64>(), d in 0u8..=18, m in multiplier(), a in 1u64..=10_000_000_000_000, b in 1u64..=10_000_000_000_000) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        if let (Ok(v_lo), Ok(v_hi)) = (collateral_value(raw, d, m, lo), collateral_value(raw, d, m, hi)) {
            prop_assert!(v_lo <= v_hi);
        }
    }

    #[test]
    fn usd_round_trip_never_creates_collateral(raw in any::<u64>(), d in 0u8..=18, m in multiplier(), p in 1u64..=10_000_000_000_000) {
        if let Ok(v) = collateral_value(raw, d, m, p) {
            if let Ok(back) = raw_for_usd(v, d, m, p) {
                prop_assert!(back <= raw);
            }
        }
    }

    #[test]
    fn effective_multiplier_boundary(now in any::<i64>(), ts in any::<i64>(), cur in any::<u128>(), new in any::<u128>()) {
        let got = effective_multiplier(now, cur, new, ts);
        prop_assert_eq!(got, if now >= ts { new } else { cur });
    }

    #[test]
    fn twap_stays_within_sample_range(prices in prop::collection::vec(1u64..=u64::MAX / 4, 1..16), gaps in prop::collection::vec(0i64..=86_400, 16), tail in 0i64..=86_400) {
        let mut ts = 1_700_000_000i64;
        let samples: Vec<Sample> = prices.iter().zip(gaps.iter()).map(|(p, g)| { ts += g; Sample { price_fp: *p, ts } }).collect();
        let now = ts + tail;
        let t = twap(&samples, now).unwrap();
        prop_assert!(t >= *prices.iter().min().unwrap());
        prop_assert!(t <= *prices.iter().max().unwrap());
    }

    #[test]
    fn band_output_is_always_inside_band(price in any::<u64>(), close in 0u64..=u64::MAX / 2, band in 0u32..=10_000) {
        let out = clamp_to_band(price, close, band).unwrap();
        let lo = (close as u128 * (10_000 - band as u128) / 10_000) as u64;
        let hi = (close as u128 * (10_000 + band as u128) / 10_000) as u64;
        prop_assert!(out >= lo && out <= hi);
    }

    #[test]
    fn change_within_cap_never_pauses(cur in multiplier(), now in any::<i64>(), ts in any::<i64>(), w in 0i64..=i64::MAX) {
        // A change of at most 1 bps against a 100 bps cap must never open the window.
        let new = cur + cur / 10_000;
        prop_assert_eq!(split_window_active(now, cur, new, ts, 100, w), Ok(false));
    }

    #[test]
    fn debt_shares_favour_the_protocol(amount in any::<u64>(), index in safu_core::lending::INDEX_SCALE..=(safu_core::lending::INDEX_SCALE * 1_000)) {
        let shares = shares_for_borrow(amount, index).unwrap();
        if let Ok(debt) = debt_for_shares(shares, index) {
            prop_assert!(debt >= amount);
        }
        prop_assert!(shares_for_repay(amount, index).unwrap() <= shares);
    }

    #[test]
    fn max_borrow_is_never_liquidatable_at_a_higher_threshold(value in any::<u64>(), ltv in 0u32..=9_000, extra in 1u32..=1_000) {
        let debt = max_borrow(value, ltv).unwrap();
        prop_assert_eq!(is_liquidatable(debt, value, ltv + extra), Ok(false));
    }

    #[test]
    fn seized_collateral_never_exceeds_value_paid_plus_bonus(repay in 0u64..=u64::MAX / 4, bonus in 0u32..=2_000, d in 0u8..=18, m in multiplier(), p in 1u64..=10_000_000_000_000) {
        if let Ok(raw) = seize_for_repay(repay, bonus, d, m, p) {
            if let Ok(v) = collateral_value(raw, d, m, p) {
                let paid_with_bonus = (repay as u128 * (10_000 + bonus as u128) / 10_000) as u64;
                prop_assert!(v <= paid_with_bonus);
            }
        }
    }

    #[test]
    fn payout_never_exceeds_any_cap(loss in any::<u64>(), base in any::<u64>(), tier in 0u32..=10_000, bal in any::<u64>(), cap in 0u32..=10_000) {
        let paid = payout(loss, base, tier, bal, cap).unwrap();
        prop_assert!(paid <= loss);
        prop_assert!(paid as u128 <= base as u128 * tier as u128 / 10_000);
        prop_assert!(paid as u128 <= bal as u128 * cap as u128 / 10_000);
    }

    #[test]
    fn loss_never_exceeds_fair_value(raw in any::<u64>(), d in 0u8..=18, m in multiplier(), p in 1u64..=10_000_000_000_000, repaid in any::<u64>()) {
        if let (Ok(loss), Ok(fair)) = (wrongful_loss(raw, d, m, p, repaid), collateral_value(raw, d, m, p)) {
            prop_assert!(loss <= fair);
        }
    }
}
