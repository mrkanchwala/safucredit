//! Regenerates `tests/vectors/safu_core.json`: the golden vectors shared by `cargo test` and `pytest`.
//!
//! Expected values come from this crate, which is the on-chain source of truth. The verdict engine's Python
//! mirror (`verdict/safu_math.py`) must reproduce every one of them, error cases included.
//!
//! The committed file is frozen. Regenerate only when the math changes on purpose, and review the diff:
//!     cargo run --example gen_vectors
//! Never regenerate to make a failing test pass — a mismatch means one side is wrong.
//!
//! Scope is the math the verdict engine needs (eng review D3). Lending math runs on-chain only.

use safu_core::{
    apply_bps, bps_diff,
    collateral::{collateral_value, effective_multiplier},
    loss::{payout, wrongful_loss},
    price::{deviation_exceeded, split_window_active, twap, Sample},
    CoreError, MULT_SCALE,
};
use serde_json::{json, Value};
use std::{fmt::Write as _, path::PathBuf};

const SEED: u64 = 0x5AFE_C0DE_2026_0914;
const RANDOM_CASES: usize = 150;

/// splitmix64: deterministic, dependency-free, identical output on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn u128(&mut self) -> u128 {
        ((self.next() as u128) << 64) | self.next() as u128
    }
    fn i64(&mut self) -> i64 {
        self.next() as i64
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    /// Realistic multiplier (0.001x to 1000x) most of the time, any u128 otherwise.
    fn multiplier(&mut self) -> u128 {
        match self.below(10) {
            0 => self.u128(),
            1 => 0,
            2 => MULT_SCALE / 1_000 + (self.u128() % (MULT_SCALE * 1_000)),
            _ => MULT_SCALE / 10 + (self.u128() % (MULT_SCALE * 10)),
        }
    }
    /// Realistic position (up to 10,000 whole shares at 8 decimals) most of the time.
    fn position(&mut self) -> u64 {
        if self.chance(15) {
            self.next()
        } else {
            self.below(1_000_000_000_000)
        }
    }
    /// Realistic price (up to $10,000, 8 decimals) most of the time.
    fn price(&mut self) -> u64 {
        if self.chance(15) {
            self.next()
        } else {
            self.below(1_000_000_000_000)
        }
    }
    /// Realistic amount (up to 1e15) most of the time, any u64 otherwise.
    fn amount(&mut self) -> u64 {
        if self.chance(25) {
            self.next()
        } else {
            self.below(1_000_000_000_000_000)
        }
    }
    fn bps(&mut self) -> u32 {
        if self.chance(15) {
            self.next() as u32
        } else {
            self.below(20_001) as u32
        }
    }
}

fn s<T: ToString>(v: T) -> Value {
    Value::String(v.to_string())
}

fn expect<T, F: FnOnce(T) -> Value>(r: Result<T, CoreError>, ok: F) -> Value {
    match r {
        Ok(v) => json!({ "ok": ok(v) }),
        Err(e) => json!({ "err": format!("{e:?}") }),
    }
}

struct Vectors(Vec<Value>);

impl Vectors {
    fn push(&mut self, func: &str, name: String, args: Value, expect: Value) {
        self.0
            .push(json!({ "fn": func, "name": name, "args": args, "expect": expect }));
    }

    fn apply_bps(&mut self, name: impl Into<String>, amount: u64, bps: u32) {
        let e = expect(apply_bps(amount, bps), s);
        self.push(
            "apply_bps",
            name.into(),
            json!({ "amount": s(amount), "bps": s(bps) }),
            e,
        );
    }

    fn bps_diff(&mut self, name: impl Into<String>, value: u64, reference: u64) {
        let e = expect(bps_diff(value, reference), s);
        self.push(
            "bps_diff",
            name.into(),
            json!({ "value": s(value), "reference": s(reference) }),
            e,
        );
    }

    fn effective_multiplier(
        &mut self,
        name: impl Into<String>,
        now: i64,
        cur: u128,
        new: u128,
        ts: i64,
    ) {
        let e = json!({ "ok": s(effective_multiplier(now, cur, new, ts)) });
        self.push(
            "effective_multiplier",
            name.into(),
            json!({ "now": s(now), "current_fp": s(cur), "new_fp": s(new), "new_effective_ts": s(ts) }),
            e,
        );
    }

    fn collateral_value(
        &mut self,
        name: impl Into<String>,
        raw: u64,
        decimals: u8,
        mult: u128,
        price: u64,
    ) {
        let e = expect(collateral_value(raw, decimals, mult, price), s);
        self.push(
            "collateral_value",
            name.into(),
            json!({ "raw": s(raw), "decimals": s(decimals), "multiplier_fp": s(mult), "price_fp": s(price) }),
            e,
        );
    }

    fn twap(&mut self, name: impl Into<String>, samples: &[Sample], now: i64) {
        let e = expect(twap(samples, now), s);
        let list: Vec<Value> = samples
            .iter()
            .map(|x| json!({ "price_fp": s(x.price_fp), "ts": s(x.ts) }))
            .collect();
        self.push(
            "twap",
            name.into(),
            json!({ "samples": list, "now": s(now) }),
            e,
        );
    }

    fn deviation_exceeded(
        &mut self,
        name: impl Into<String>,
        new_price: u64,
        reference: u64,
        cap: u32,
    ) {
        let e = expect(deviation_exceeded(new_price, reference, cap), Value::Bool);
        self.push(
            "deviation_exceeded",
            name.into(),
            json!({ "new_price": s(new_price), "reference": s(reference), "cap_bps": s(cap) }),
            e,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn split_window_active(
        &mut self,
        name: impl Into<String>,
        now: i64,
        cur: u128,
        new: u128,
        ts: i64,
        cap: u32,
        window: i64,
    ) {
        let e = expect(
            split_window_active(now, cur, new, ts, cap, window),
            Value::Bool,
        );
        self.push(
            "split_window_active",
            name.into(),
            json!({
                "now": s(now), "current_fp": s(cur), "new_fp": s(new),
                "new_effective_ts": s(ts), "cap_bps": s(cap), "window_secs": s(window)
            }),
            e,
        );
    }

    fn wrongful_loss(
        &mut self,
        name: impl Into<String>,
        raw: u64,
        decimals: u8,
        mult: u128,
        reference: u64,
        debt: u64,
    ) {
        let e = expect(wrongful_loss(raw, decimals, mult, reference, debt), s);
        self.push(
            "wrongful_loss",
            name.into(),
            json!({
                "seized_raw": s(raw), "decimals": s(decimals), "multiplier_fp": s(mult),
                "reference_price_fp": s(reference), "debt_repaid": s(debt)
            }),
            e,
        );
    }

    fn payout(
        &mut self,
        name: impl Into<String>,
        loss: u64,
        base: u64,
        tier: u32,
        balance: u64,
        per_claim: u32,
    ) {
        let e = expect(payout(loss, base, tier, balance, per_claim), s);
        self.push(
            "payout",
            name.into(),
            json!({
                "loss": s(loss), "ceiling_base_value": s(base), "tier_ceiling_bps": s(tier),
                "backstop_balance": s(balance), "per_claim_cap_bps": s(per_claim)
            }),
            e,
        );
    }
}

// AAPLx as read on mainnet 2026-09-13/14.
const AAPLX_DECIMALS: u8 = 8;
const AAPLX_MULT: u128 = 1_002_664_200_000;
const AAPLX_NEXT_MULT: u128 = 1_003_269_000_000;
const PRICE_330_28: u64 = 33_028_000_000;

fn hand_picked(v: &mut Vectors) {
    v.apply_bps("half", 1_000, 5_000);
    v.apply_bps("rounds down", 999, 1);
    v.apply_bps("zero bps", u64::MAX, 0);
    v.apply_bps("full at u64 max", u64::MAX, 10_000);
    v.apply_bps("above 100% overflows u64", u64::MAX, 10_001);
    v.apply_bps("u32 max bps on small amount", 1, u32::MAX);

    v.bps_diff("equal", 10_000, 10_000);
    v.bps_diff("5% up", 10_500, 10_000);
    v.bps_diff("symmetric down", 9_500, 10_000);
    v.bps_diff("rounds down", 10_001, 30_000);
    v.bps_diff("zero reference", 1, 0);
    v.bps_diff("u64 extremes", u64::MAX, 1);

    let ts = 1_760_000_000;
    v.effective_multiplier("one second before", ts - 1, AAPLX_MULT, AAPLX_NEXT_MULT, ts);
    v.effective_multiplier(
        "exactly at timestamp uses new",
        ts,
        AAPLX_MULT,
        AAPLX_NEXT_MULT,
        ts,
    );
    v.effective_multiplier("one second after", ts + 1, AAPLX_MULT, AAPLX_NEXT_MULT, ts);
    v.effective_multiplier("i64 extremes", i64::MIN, 1, u128::MAX, i64::MAX);

    v.collateral_value(
        "one AAPLx share at $330.28",
        100_000_000,
        AAPLX_DECIMALS,
        AAPLX_MULT,
        PRICE_330_28,
    );
    v.collateral_value(
        "before 4:1 split",
        100_000_000,
        8,
        MULT_SCALE,
        40_000_000_000,
    );
    v.collateral_value(
        "after 4:1 split",
        100_000_000,
        8,
        4 * MULT_SCALE,
        10_000_000_000,
    );
    v.collateral_value(
        "desync price first",
        100_000_000,
        8,
        MULT_SCALE,
        10_000_000_000,
    );
    v.collateral_value(
        "desync multiplier first",
        100_000_000,
        8,
        4 * MULT_SCALE,
        40_000_000_000,
    );
    v.collateral_value("zero multiplier", 1, 8, 0, 1);
    v.collateral_value("zero price", 100_000_000, 8, MULT_SCALE, 0);
    v.collateral_value("dust rounds to zero", 1, 8, AAPLX_MULT, PRICE_330_28);
    v.collateral_value("zero decimals", 3, 0, MULT_SCALE, 12_345_678_901);
    v.collateral_value(
        "18 decimals",
        1_000_000_000_000_000_000,
        18,
        MULT_SCALE,
        33_028_000_000,
    );
    v.collateral_value(
        "u64 max raw at 1000x overflows u128",
        u64::MAX,
        8,
        MULT_SCALE * 1_000,
        u64::MAX,
    );
    v.collateral_value("u64 max raw at 1x", u64::MAX, 8, MULT_SCALE, 1);
    v.collateral_value(
        "result overflows u64",
        u64::MAX,
        0,
        MULT_SCALE,
        100_000_000_000,
    );
    v.collateral_value("multiplier u128 max overflows", 2, 8, u128::MAX, 1);
    v.collateral_value("decimals 38 ok", u64::MAX, 38, MULT_SCALE, u64::MAX);
    v.collateral_value("decimals 39 overflows pow10", 1, 39, MULT_SCALE, 1);
    v.collateral_value("decimals 255 overflows pow10", 1, 255, MULT_SCALE, 1);

    let smp = |price_fp, ts| Sample { price_fp, ts };
    v.twap("single sample", &[smp(33_028_000_000, 100)], 400);
    v.twap("weights by time held", &[smp(100, 0), smp(200, 30)], 40);
    v.twap("zero duration returns last", &[smp(7, 5), smp(9, 5)], 5);
    v.twap("empty", &[], 10);
    v.twap("unsorted", &[smp(1, 20), smp(2, 10)], 30);
    v.twap("now before last sample", &[smp(1, 20)], 10);
    v.twap("rounds down", &[smp(1, 0), smp(2, 1), smp(2, 2)], 3);
    v.twap("i64 span overflows", &[smp(1, i64::MIN)], i64::MAX);
    v.twap(
        "weighted sum overflows u128",
        &[smp(u64::MAX, 0), smp(u64::MAX, i64::MAX)],
        i64::MAX,
    );
    v.twap(
        "sub-cap drift, 16 samples",
        &(0..16)
            .map(|i| smp(33_028_000_000 - i as u64 * 40_000_000, i * 300))
            .collect::<Vec<_>>(),
        4_800,
    );

    v.deviation_exceeded("exactly at cap is not exceeded", 10_500, 10_000, 500);
    v.deviation_exceeded("one past cap", 10_501, 10_000, 500);
    v.deviation_exceeded("below reference past cap", 9_499, 10_000, 500);
    v.deviation_exceeded("zero reference", 1, 0, 500);
    v.deviation_exceeded("zero cap any move", 10_001, 10_000, 0);
    v.deviation_exceeded(
        "live AAPLx 1% move under 5% cap",
        33_358_280_000,
        PRICE_330_28,
        500,
    );

    let w = 3_600;
    for (label, now) in [
        ("split window: before start", ts - w - 1),
        ("split window: at start", ts - w),
        ("split window: at timestamp", ts),
        ("split window: at end", ts + w),
        ("split window: after end", ts + w + 1),
    ] {
        v.split_window_active(label, now, MULT_SCALE, 4 * MULT_SCALE, ts, 500, w);
    }
    v.split_window_active(
        "reverse split pauses",
        0,
        4 * MULT_SCALE,
        MULT_SCALE,
        0,
        500,
        60,
    );
    v.split_window_active(
        "AAPLx dividend step never pauses",
        ts,
        AAPLX_MULT,
        AAPLX_NEXT_MULT,
        ts,
        500,
        w,
    );
    v.split_window_active(
        "change exactly at cap does not pause",
        ts,
        10_000,
        10_500,
        ts,
        500,
        w,
    );
    v.split_window_active("change one past cap pauses", ts, 10_000, 10_501, ts, 500, w);
    v.split_window_active("zero current multiplier", ts, 0, MULT_SCALE, ts, 500, w);
    v.split_window_active("zero new multiplier", ts, MULT_SCALE, 0, ts, 500, w);
    v.split_window_active(
        "negative window",
        ts,
        MULT_SCALE,
        4 * MULT_SCALE,
        ts,
        500,
        -1,
    );
    v.split_window_active(
        "zero window only at timestamp",
        ts,
        MULT_SCALE,
        4 * MULT_SCALE,
        ts,
        500,
        0,
    );
    v.split_window_active(
        "window saturates at i64 min",
        i64::MIN,
        MULT_SCALE,
        4 * MULT_SCALE,
        i64::MIN + 5,
        500,
        10,
    );
    v.split_window_active(
        "window saturates at i64 max",
        i64::MAX,
        MULT_SCALE,
        4 * MULT_SCALE,
        i64::MAX - 5,
        500,
        10,
    );
    v.split_window_active("change overflows u128", ts, 1, u128::MAX, ts, 500, w);

    v.wrongful_loss(
        "fair value minus debt repaid",
        100_000_000,
        8,
        MULT_SCALE,
        40_000_000_000,
        250_000_000,
    );
    v.wrongful_loss(
        "floors at zero",
        100_000_000,
        8,
        MULT_SCALE,
        10_000_000_000,
        250_000_000,
    );
    v.wrongful_loss(
        "exactly zero",
        100_000_000,
        8,
        MULT_SCALE,
        25_000_000_000,
        250_000_000,
    );
    v.wrongful_loss(
        "AAPLx with live multiplier",
        250_000_000,
        AAPLX_DECIMALS,
        AAPLX_MULT,
        PRICE_330_28,
        600_000_000,
    );
    v.wrongful_loss(
        "price-first split desync seized 4x too much",
        400_000_000,
        8,
        4 * MULT_SCALE,
        10_000_000_000,
        100_000_000,
    );
    v.wrongful_loss("valuation error propagates", 1, 8, 0, 1, 0);
    v.wrongful_loss(
        "overflow propagates",
        u64::MAX,
        8,
        MULT_SCALE * 1_000,
        u64::MAX,
        0,
    );

    v.payout("loss is smallest", 150, 1_000, 10_000, 10_000, 10_000);
    v.payout(
        "tier ceiling is smallest",
        150,
        1_000,
        1_000,
        10_000,
        10_000,
    );
    v.payout("per-claim cap is smallest", 150, 1_000, 10_000, 1_000, 500);
    v.payout(
        "tier C 50% ceiling",
        900_000_000,
        1_000_000_000,
        5_000,
        100_000_000_000,
        5_000,
    );
    v.payout("empty backstop pays nothing", 150, 1_000, 10_000, 0, 5_000);
    v.payout("zero loss", 0, 1_000, 10_000, 10_000, 5_000);
    v.payout("tier cap overflows u64", 1, u64::MAX, 20_000, 1, 1);
    v.payout("backstop cap overflows u64", 1, 1, 1, u64::MAX, 20_000);
}

fn random(v: &mut Vectors, r: &mut Rng) {
    for i in 0..RANDOM_CASES {
        let amount = r.amount();
        let bps = r.bps();
        v.apply_bps(format!("random {i}"), amount, bps);

        let value = r.amount();
        let reference = if r.chance(5) { 0 } else { r.amount() };
        v.bps_diff(format!("random {i}"), value, reference);

        let ts = r.i64();
        let now = if r.chance(50) {
            ts.saturating_add(r.below(7) as i64 - 3)
        } else {
            r.i64()
        };
        let (cur, new) = (r.multiplier(), r.multiplier());
        v.effective_multiplier(format!("random {i}"), now, cur, new, ts);

        let decimals = if r.chance(10) {
            r.below(256) as u8
        } else {
            r.below(19) as u8
        };
        let (raw, mult, price) = (r.position(), r.multiplier(), r.price());
        v.collateral_value(format!("random {i}"), raw, decimals, mult, price);

        let n = r.below(17) as usize;
        let mut t = r.below(2_000_000_000) as i64;
        let samples: Vec<Sample> = (0..n)
            .map(|_| {
                t = if r.chance(3) {
                    t - r.below(600) as i64
                } else {
                    t + r.below(900) as i64
                };
                let price_fp = if r.chance(10) {
                    r.next()
                } else {
                    30_000_000_000 + r.below(4_000_000_000)
                };
                Sample { price_fp, ts: t }
            })
            .collect();
        let now = if r.chance(5) {
            t - 1
        } else {
            t + r.below(900) as i64
        };
        v.twap(format!("random {i}"), &samples, now);

        let reference = if r.chance(5) { 0 } else { r.amount() };
        let new_price = if r.chance(50) {
            let step = r.below(reference / 10 + 1);
            if r.chance(50) {
                reference.saturating_add(step)
            } else {
                reference.saturating_sub(step)
            }
        } else {
            r.amount()
        };
        let cap = r.below(2_001) as u32;
        v.deviation_exceeded(format!("random {i}"), new_price, reference, cap);

        let ts = r.below(4_000_000_000) as i64;
        let window = if r.chance(5) {
            -(r.below(100) as i64) - 1
        } else {
            r.below(7_201) as i64
        };
        let now = ts + r.below(20_000) as i64 - 10_000;
        let cur = r.multiplier();
        let new = match r.below(4) {
            0 => cur.saturating_mul(r.below(5) as u128 + 1),
            1 => cur / (r.below(5) as u128 + 1),
            2 => cur.saturating_add(cur / 1_000),
            _ => r.multiplier(),
        };
        v.split_window_active(format!("random {i}"), now, cur, new, ts, cap, window);

        let decimals = if r.chance(10) {
            r.below(256) as u8
        } else {
            r.below(19) as u8
        };
        let (seized, mult, reference) = (r.position(), r.multiplier(), r.price());
        let debt = if r.chance(15) {
            r.next()
        } else {
            r.below(5_000_000_000_000)
        };
        v.wrongful_loss(
            format!("random {i}"),
            seized,
            decimals,
            mult,
            reference,
            debt,
        );

        let (loss, base, balance) = (r.amount(), r.amount(), r.amount());
        let (tier, per_claim) = (r.bps(), r.bps());
        v.payout(format!("random {i}"), loss, base, tier, balance, per_claim);
    }
}

fn main() {
    let mut v = Vectors(Vec::new());
    hand_picked(&mut v);
    random(&mut v, &mut Rng(SEED));

    // One case per line keeps diffs reviewable when the math changes on purpose.
    let mut out = String::new();
    writeln!(out, "{{").unwrap();
    writeln!(out, "  \"schema\": 1,").unwrap();
    writeln!(
        out,
        "  \"generator\": \"crates/safu-core/examples/gen_vectors.rs (safu-core {}), seed {SEED:#x}\",",
        env!("CARGO_PKG_VERSION")
    )
    .unwrap();
    writeln!(out, "  \"integers\": \"every integer is a decimal string; u128 does not survive JSON numbers\",").unwrap();
    writeln!(out, "  \"cases\": [").unwrap();
    for (i, case) in v.0.iter().enumerate() {
        let sep = if i + 1 == v.0.len() { "" } else { "," };
        writeln!(out, "    {case}{sep}").unwrap();
    }
    writeln!(out, "  ]").unwrap();
    writeln!(out, "}}").unwrap();

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors/safu_core.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, out).unwrap();
    eprintln!("wrote {} cases to {}", v.0.len(), path.display());
}
