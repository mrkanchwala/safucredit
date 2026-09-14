//! Golden vectors: safu-core must still produce every result in `tests/vectors/safu_core.json`.
//!
//! The same file is asserted by the verdict engine's pytest suite (`verdict/tests/test_golden.py`), so the
//! on-chain math and the off-chain verdict math cannot drift apart unnoticed (eng review D3).
//!
//! A failure here means the Rust math changed. If the change was deliberate, regenerate with
//! `cargo run --example gen_vectors`, review the diff, and the Python mirror must then be updated to match.

use safu_core::{
    apply_bps, bps_diff,
    collateral::{collateral_value, effective_multiplier},
    loss::{payout, wrongful_loss},
    price::{deviation_exceeded, split_window_active, twap, Sample},
    CoreError,
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt::Debug, str::FromStr};

const VECTORS: &str = include_str!("../../../tests/vectors/safu_core.json");

fn arg<T: FromStr>(args: &Value, key: &str) -> T
where
    T::Err: Debug,
{
    args[key]
        .as_str()
        .unwrap_or_else(|| panic!("arg {key} missing or not a string"))
        .parse()
        .unwrap_or_else(|e| panic!("arg {key}: {e:?}"))
}

fn encode<T: ToString>(r: Result<T, CoreError>) -> Value {
    match r {
        Ok(v) => serde_json::json!({ "ok": v.to_string() }),
        Err(e) => serde_json::json!({ "err": format!("{e:?}") }),
    }
}

fn encode_bool(r: Result<bool, CoreError>) -> Value {
    match r {
        Ok(v) => serde_json::json!({ "ok": v }),
        Err(e) => serde_json::json!({ "err": format!("{e:?}") }),
    }
}

fn run(func: &str, a: &Value) -> Value {
    match func {
        "apply_bps" => encode(apply_bps(arg(a, "amount"), arg(a, "bps"))),
        "bps_diff" => encode(bps_diff(arg(a, "value"), arg(a, "reference"))),
        "effective_multiplier" => encode::<u128>(Ok(effective_multiplier(
            arg(a, "now"),
            arg(a, "current_fp"),
            arg(a, "new_fp"),
            arg(a, "new_effective_ts"),
        ))),
        "collateral_value" => encode(collateral_value(
            arg(a, "raw"),
            arg(a, "decimals"),
            arg(a, "multiplier_fp"),
            arg(a, "price_fp"),
        )),
        "twap" => {
            let samples: Vec<Sample> = a["samples"]
                .as_array()
                .expect("samples array")
                .iter()
                .map(|s| Sample {
                    price_fp: arg(s, "price_fp"),
                    ts: arg(s, "ts"),
                })
                .collect();
            encode(twap(&samples, arg(a, "now")))
        }
        "deviation_exceeded" => encode_bool(deviation_exceeded(
            arg(a, "new_price"),
            arg(a, "reference"),
            arg(a, "cap_bps"),
        )),
        "split_window_active" => encode_bool(split_window_active(
            arg(a, "now"),
            arg(a, "current_fp"),
            arg(a, "new_fp"),
            arg(a, "new_effective_ts"),
            arg(a, "cap_bps"),
            arg(a, "window_secs"),
        )),
        "wrongful_loss" => encode(wrongful_loss(
            arg(a, "seized_raw"),
            arg(a, "decimals"),
            arg(a, "multiplier_fp"),
            arg(a, "reference_price_fp"),
            arg(a, "debt_repaid"),
        )),
        "payout" => encode(payout(
            arg(a, "loss"),
            arg(a, "ceiling_base_value"),
            arg(a, "tier_ceiling_bps"),
            arg(a, "backstop_balance"),
            arg(a, "per_claim_cap_bps"),
        )),
        other => panic!("vector names a function this test does not know: {other}"),
    }
}

#[test]
fn safu_core_matches_every_golden_vector() {
    let doc: Value = serde_json::from_str(VECTORS).expect("vectors parse");
    assert_eq!(doc["schema"], 1);
    let cases = doc["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty());

    let mut per_fn: BTreeMap<&str, usize> = BTreeMap::new();
    let mut failures = Vec::new();
    for case in cases {
        let func = case["fn"].as_str().expect("fn");
        *per_fn.entry(func).or_default() += 1;
        let got = run(func, &case["args"]);
        if got != case["expect"] {
            failures.push(format!(
                "{func}:{} expected {} got {got}",
                case["name"], case["expect"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} vectors failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );

    let expected = [
        "apply_bps",
        "bps_diff",
        "collateral_value",
        "deviation_exceeded",
        "effective_multiplier",
        "payout",
        "split_window_active",
        "twap",
        "wrongful_loss",
    ];
    assert_eq!(per_fn.keys().copied().collect::<Vec<_>>(), expected);
}
