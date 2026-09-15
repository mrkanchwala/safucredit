//! Coverage-guided fuzz target for `stock_vault::logic::requote`, the split-adjustment function at
//! the center of the 2026-09-15 reverse-split wrongful-liquidation fix. The deterministic 2,000-case
//! sweep in `logic.rs` and the `proptest` chained-requote property both already cover this function;
//! this target exists to let libFuzzer search for inputs neither of those picked (they generate
//! uniformly, libFuzzer searches adversarially and remembers a corpus).
//!
//! Runs only in CI / a codespace — libFuzzer's ASan runtime crashes on macOS arm64 before any
//! iteration, a documented host issue unrelated to this code (see context/tools/github.md §7).

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use stock_vault::logic::requote;

#[derive(Arbitrary, Debug)]
struct Input {
    price: u64,
    from_fp: u128,
    to_fp: u128,
}

fuzz_target!(|input: Input| {
    let Input { price, from_fp, to_fp } = input;

    let result = requote(price, from_fp, to_fp);

    // requote's own documented contract: 0 in, 0 out, always -- checked first regardless of the
    // multipliers, since a zero price carries no value to preserve.
    if price == 0 {
        assert_eq!(result.unwrap(), 0, "a zero price must always requote to zero");
        return;
    }

    // Invalid multipliers are the only other rejection path -- anything else must succeed.
    if from_fp == 0 || to_fp == 0 {
        assert!(result.is_err(), "a zero multiplier must be rejected, not silently accepted");
        return;
    }

    match result {
        Ok(out) => {
            // Never zero for a non-zero input price (that would erase the position's value).
            assert_ne!(out, 0, "requote({price}, {from_fp}, {to_fp}) produced 0 for a non-zero price");

            // Value-invariance within rounding, same bound the deterministic sweep already proves:
            // off by at most half a unit of the output scale, or clamped to the floor of 1.
            let before = (price as u128).saturating_mul(from_fp);
            let after = (out as u128).saturating_mul(to_fp);
            let tolerance = to_fp / 2;
            assert!(
                before.abs_diff(after) <= tolerance || out == 1,
                "requote({price}, {from_fp}, {to_fp}) = {out}: value drifted from {before} to {after}, \
                 tolerance {tolerance}"
            );
        }
        Err(_) => {
            // The only legitimate failure left is overflow in the u64 cast -- confirm that's
            // plausible for these inputs rather than accepting an unexplained rejection.
            let would_overflow = (price as u128)
                .checked_mul(from_fp)
                .and_then(|v| v.checked_add(to_fp / 2))
                .map(|v| v / to_fp)
                .is_none_or(|scaled| u64::try_from(scaled).is_err());
            assert!(
                would_overflow,
                "requote({price}, {from_fp}, {to_fp}) rejected without an overflow reason"
            );
        }
    }
});
