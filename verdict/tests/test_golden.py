"""Golden vectors: the Python verdict math must match safu-core, case for case (eng review D3).

The same file is asserted by `crates/safu-core/tests/golden.rs`. If this fails, one side of the math has
drifted. Find which one is wrong. Never regenerate the vectors to make the failure go away.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "verdict"))

import safu_math  # noqa: E402
from safu_math import CoreError, Sample  # noqa: E402

VECTORS = json.loads((ROOT / "tests" / "vectors" / "safu_core.json").read_text())
CASES = VECTORS["cases"]


def _int_args(args: dict) -> dict:
    return {k: int(v) for k, v in args.items()}


def _twap(args: dict):
    samples = [Sample(int(s["price_fp"]), int(s["ts"])) for s in args["samples"]]
    return safu_math.twap(samples, int(args["now"]))


# Every function the vectors exercise must be listed here. An unlisted one fails loudly rather than skipping.
CALLS = {
    "apply_bps": lambda a: safu_math.apply_bps(**_int_args(a)),
    "bps_diff": lambda a: safu_math.bps_diff(**_int_args(a)),
    "effective_multiplier": lambda a: safu_math.effective_multiplier(**_int_args(a)),
    "collateral_value": lambda a: safu_math.collateral_value(**_int_args(a)),
    "twap": _twap,
    "deviation_exceeded": lambda a: safu_math.deviation_exceeded(**_int_args(a)),
    "split_window_active": lambda a: safu_math.split_window_active(**_int_args(a)),
    "wrongful_loss": lambda a: safu_math.wrongful_loss(**_int_args(a)),
    "payout": lambda a: safu_math.payout(**_int_args(a)),
}


def test_schema_version():
    assert VECTORS["schema"] == 1


def test_every_vector_function_is_mirrored_and_every_mirror_is_covered():
    in_vectors = {c["fn"] for c in CASES}
    assert in_vectors == set(CALLS), (
        f"not mirrored: {in_vectors - set(CALLS)}; no vectors: {set(CALLS) - in_vectors}"
    )


def test_every_function_has_success_and_error_cases():
    for fn in CALLS:
        outcomes = {
            "err" if "err" in c["expect"] else "ok" for c in CASES if c["fn"] == fn
        }
        # effective_multiplier cannot fail in Rust, so it is the one function with no error cases.
        expected = {"ok"} if fn == "effective_multiplier" else {"ok", "err"}
        assert outcomes == expected, f"{fn}: {outcomes}"


@pytest.mark.parametrize("case", CASES, ids=[f"{c['fn']}:{c['name']}" for c in CASES])
def test_matches_rust(case):
    call = CALLS[case["fn"]]
    expect = case["expect"]
    if "err" in expect:
        with pytest.raises(CoreError) as info:
            call(case["args"])
        assert info.value.kind == expect["err"]
        return
    got = call(case["args"])
    want = expect["ok"]
    if isinstance(want, bool):
        assert got is want
    else:
        assert not isinstance(got, bool) and got == int(want)


@pytest.mark.parametrize(
    "call",
    [
        lambda: safu_math.apply_bps(-1, 1),
        lambda: safu_math.apply_bps(1 << 64, 1),
        lambda: safu_math.collateral_value(1, 256, 1, 1),
        lambda: safu_math.collateral_value(1, 8, 1.0, 1),
        lambda: safu_math.split_window_active(1 << 63, 1, 1, 0, 0, 0),
        lambda: safu_math.payout(True, 1, 1, 1, 1),
    ],
)
def test_inputs_rust_could_never_receive_are_rejected(call):
    with pytest.raises(ValueError):
        call()
