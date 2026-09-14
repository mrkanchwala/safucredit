"""Python mirror of the safu-core math the verdict engine needs.

The Rust crate `crates/safu-core` is the source of truth: it is what the on-chain programs run. This module
must agree with it on every input, including which error comes back and when. Agreement is enforced by the
golden vectors in `tests/vectors/safu_core.json`, asserted by both `cargo test` and `pytest` (eng review D3).

Rules copied from the crate, so the two cannot drift silently:
- Integers only. No floats anywhere.
- Rust's checked u128 arithmetic is reproduced: an intermediate past u128, or a result past u64, raises
  `CoreError("Overflow")` exactly where Rust returns `Err(Overflow)`.
- Rounding favours the protocol: collateral value rounds down.

Inputs outside the Rust parameter types (a negative amount, a u64 above 2^64 - 1) cannot reach the Rust
functions at all, so they raise `ValueError` here rather than a `CoreError`.

Scope: valuation, the price guards the verdict checks against, loss and payout. Lending math (rates, debt
shares, liquidation bonus) runs on-chain only and is deliberately not mirrored.
"""

from __future__ import annotations

from dataclasses import dataclass

BPS = 10_000
MULT_SCALE = 1_000_000_000_000
PRICE_DECIMALS = 8
USD_DECIMALS = 6

U8_MAX = (1 << 8) - 1
U32_MAX = (1 << 32) - 1
U64_MAX = (1 << 64) - 1
U128_MAX = (1 << 128) - 1
I64_MIN = -(1 << 63)
I64_MAX = (1 << 63) - 1


class CoreError(Exception):
    """Mirror of `safu_core::CoreError`. `kind` is the Rust variant name."""

    KINDS = frozenset(
        {
            "Overflow",
            "DivideByZero",
            "EmptySamples",
            "UnsortedSamples",
            "InvalidMultiplier",
            "InvalidParameter",
        }
    )

    def __init__(self, kind: str) -> None:
        if kind not in self.KINDS:
            raise ValueError(f"unknown CoreError kind: {kind}")
        super().__init__(kind)
        self.kind = kind


@dataclass(frozen=True)
class Sample:
    """USD per whole share (8 decimals) observed at unix second `ts`."""

    price_fp: int
    ts: int


# --- parameter type checks (the Rust type system, made explicit) -------------------------------------------


def _typed(value: int, lo: int, hi: int, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{name} must be an int, got {type(value).__name__}")
    if not lo <= value <= hi:
        raise ValueError(f"{name}={value} is outside [{lo}, {hi}]")
    return value


def _u8(v: int, name: str) -> int:
    return _typed(v, 0, U8_MAX, name)


def _u32(v: int, name: str) -> int:
    return _typed(v, 0, U32_MAX, name)


def _u64(v: int, name: str) -> int:
    return _typed(v, 0, U64_MAX, name)


def _u128(v: int, name: str) -> int:
    return _typed(v, 0, U128_MAX, name)


def _i64(v: int, name: str) -> int:
    return _typed(v, I64_MIN, I64_MAX, name)


# --- checked u128 arithmetic, as in safu-core's lib.rs ----------------------------------------------------


def _mul(a: int, b: int) -> int:
    r = a * b
    if r > U128_MAX:
        raise CoreError("Overflow")
    return r


def _add(a: int, b: int) -> int:
    r = a + b
    if r > U128_MAX:
        raise CoreError("Overflow")
    return r


def _div(a: int, b: int) -> int:
    if b == 0:
        raise CoreError("DivideByZero")
    return a // b


def _to_u64(v: int) -> int:
    if v > U64_MAX:
        raise CoreError("Overflow")
    return v


def _pow10(n: int) -> int:
    r = 10**n
    if r > U128_MAX:
        raise CoreError("Overflow")
    return r


def _i64_checked_sub(a: int, b: int) -> int:
    r = a - b
    if not I64_MIN <= r <= I64_MAX:
        raise CoreError("Overflow")
    return r


def _i64_saturating_add(a: int, b: int) -> int:
    return max(I64_MIN, min(I64_MAX, a + b))


# --- lib.rs -----------------------------------------------------------------------------------------------


def apply_bps(amount: int, bps: int) -> int:
    """`amount × bps / 10_000`, rounded down."""
    _u64(amount, "amount")
    _u32(bps, "bps")
    return _to_u64(_div(_mul(amount, bps), BPS))


def bps_diff(value: int, reference: int) -> int:
    """Absolute difference between `value` and `reference`, in basis points of `reference`."""
    _u64(value, "value")
    _u64(reference, "reference")
    return _div(_mul(abs(value - reference), BPS), reference)


# --- collateral.rs ----------------------------------------------------------------------------------------


def effective_multiplier(
    now: int, current_fp: int, new_fp: int, new_effective_ts: int
) -> int:
    """Multiplier in effect at `now`: the new one applies from `now >= new_effective_ts`."""
    _i64(now, "now")
    _u128(current_fp, "current_fp")
    _u128(new_fp, "new_fp")
    _i64(new_effective_ts, "new_effective_ts")
    return new_fp if now >= new_effective_ts else current_fp


def collateral_value(raw: int, decimals: int, multiplier_fp: int, price_fp: int) -> int:
    """USD value (6 decimals) of `raw` base units, at `multiplier_fp` and `price_fp` (8 decimals). Rounds down."""
    _u64(raw, "raw")
    _u8(decimals, "decimals")
    _u128(multiplier_fp, "multiplier_fp")
    _u64(price_fp, "price_fp")
    if multiplier_fp == 0:
        raise CoreError("InvalidMultiplier")
    scaled_raw = _div(_mul(raw, multiplier_fp), MULT_SCALE)
    usd_price_units = _div(_mul(scaled_raw, price_fp), _pow10(decimals))
    return _to_u64(_div(usd_price_units, _pow10(PRICE_DECIMALS - USD_DECIMALS)))


# --- price.rs ---------------------------------------------------------------------------------------------


def twap(samples: list[Sample], now: int) -> int:
    """Time-weighted average price. Each sample holds until the next one's timestamp; the last until `now`."""
    for i, smp in enumerate(samples):
        _u64(smp.price_fp, f"samples[{i}].price_fp")
        _i64(smp.ts, f"samples[{i}].ts")
    _i64(now, "now")
    if not samples:
        raise CoreError("EmptySamples")
    weighted = 0
    total = 0
    for i, smp in enumerate(samples):
        end = samples[i + 1].ts if i + 1 < len(samples) else now
        dt = _i64_checked_sub(end, smp.ts)
        if dt < 0:
            raise CoreError("UnsortedSamples")
        weighted = _add(weighted, _mul(smp.price_fp, dt))
        total = _add(total, dt)
    if total == 0:
        return samples[-1].price_fp
    return _to_u64(_div(weighted, total))


def deviation_exceeded(new_price: int, reference: int, cap_bps: int) -> bool:
    """True when `new_price` moves more than `cap_bps` away from `reference`. Exactly at the cap is not."""
    _u64(new_price, "new_price")
    _u64(reference, "reference")
    _u32(cap_bps, "cap_bps")
    return bps_diff(new_price, reference) > cap_bps


def split_window_active(
    now: int,
    current_fp: int,
    new_fp: int,
    new_effective_ts: int,
    cap_bps: int,
    window_secs: int,
) -> bool:
    """True while liquidations pause around a multiplier change larger than `cap_bps`, within `window_secs`
    of the effective timestamp on either side (edges inclusive)."""
    _i64(now, "now")
    _u128(current_fp, "current_fp")
    _u128(new_fp, "new_fp")
    _i64(new_effective_ts, "new_effective_ts")
    _u32(cap_bps, "cap_bps")
    _i64(window_secs, "window_secs")
    if current_fp == 0 or new_fp == 0:
        raise CoreError("InvalidMultiplier")
    if window_secs < 0:
        raise CoreError("InvalidParameter")
    change_bps = _div(_mul(abs(current_fp - new_fp), BPS), current_fp)
    if change_bps <= cap_bps:
        return False
    start = _i64_saturating_add(new_effective_ts, -window_secs)
    end = _i64_saturating_add(new_effective_ts, window_secs)
    return start <= now <= end


# --- loss.rs ----------------------------------------------------------------------------------------------


def wrongful_loss(
    seized_raw: int,
    decimals: int,
    multiplier_fp: int,
    reference_price_fp: int,
    debt_repaid: int,
) -> int:
    """`collateral seized × reference price − debt repaid`, floored at zero (spec 13d)."""
    _u64(debt_repaid, "debt_repaid")
    fair_value = collateral_value(
        seized_raw, decimals, multiplier_fp, reference_price_fp
    )
    return max(fair_value - debt_repaid, 0)


def payout(
    loss: int,
    ceiling_base_value: int,
    tier_ceiling_bps: int,
    backstop_balance: int,
    per_claim_cap_bps: int,
) -> int:
    """The loss 1:1, never above the tier ceiling nor the per-claim share of the backstop."""
    _u64(loss, "loss")
    tier_cap = apply_bps(ceiling_base_value, tier_ceiling_bps)
    backstop_cap = apply_bps(backstop_balance, per_claim_cap_bps)
    return min(loss, tier_cap, backstop_cap)
