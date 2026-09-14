"""Verdict engine tests.

encode_message must byte-match solana/programs/backstop/src/verdict.rs::encode_message exactly.
The vectors in test_message_* / test_every_field_changes_the_message are ported from that file's
own #[cfg(test)] module (`args()`, `message_is_exactly_178_bytes_and_starts_with_domain`,
`every_field_changes_the_message`) -- not invented here. If the Rust side changes, port the change,
do not just make this pass.
"""

from __future__ import annotations

import sys
from dataclasses import replace
from datetime import UTC, datetime
from pathlib import Path

import pytest
from solders.keypair import Keypair
from solders.pubkey import Pubkey

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from engine import (  # noqa: E402
    CLUSTER_DEVNET,
    CLUSTER_MAINNET,
    DOMAIN,
    MAX_AFTER_WAIT_SECS,
    MESSAGE_LEN,
    MIN_AFTER_WAIT_SECS,
    FactsArgs,
    VerdictEngineError,
    compute_evidence_hash,
    encode_message,
    price_to_fp,
    reference_after,
    reference_at,
    sign_verdict,
)


def _pk(byte: int) -> Pubkey:
    return Pubkey(bytes([byte] * 32))


def _args() -> FactsArgs:
    return FactsArgs(
        liquidation_record=_pk(7),
        borrower=_pk(8),
        ref_at_liq=40_000_000_000,
        ref_after=40_100_000_000,
        after_ts=1_000_003_600,
        evidence_hash=bytes([9] * 32),
        deadline=1_800_000_000,
    )


def _iso(ts: int) -> str:
    return datetime.fromtimestamp(ts, tz=UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- message encoding, ported from verdict.rs #[cfg(test)] --------------------------------------


def test_message_is_exactly_178_bytes_and_starts_with_domain():
    m = encode_message(_pk(1), CLUSTER_DEVNET, _args())
    assert len(m) == MESSAGE_LEN == 178
    assert m[:17] == DOMAIN


def test_every_field_changes_the_message():
    pid = _pk(1)
    base = encode_message(pid, CLUSTER_DEVNET, _args())
    variants = [
        encode_message(_pk(2), CLUSTER_DEVNET, _args()),
        encode_message(pid, CLUSTER_MAINNET, _args()),
    ]
    base_args = _args()
    mutated_hash = bytearray(base_args.evidence_hash)
    mutated_hash[0] ^= 1
    field_edits = [
        {"liquidation_record": _pk(70)},
        {"borrower": _pk(80)},
        {"ref_at_liq": base_args.ref_at_liq + 1},
        {"ref_after": base_args.ref_after + 1},
        {"after_ts": base_args.after_ts + 1},
        {"evidence_hash": bytes(mutated_hash)},
        {"deadline": base_args.deadline + 1},
    ]
    for edit in field_edits:
        variants.append(encode_message(pid, CLUSTER_DEVNET, replace(base_args, **edit)))
    for v in variants:
        assert v != base


def test_facts_args_rejects_a_short_evidence_hash():
    with pytest.raises(ValueError):
        replace(_args(), evidence_hash=b"\x00" * 31)


def test_facts_args_rejects_an_out_of_range_u64():
    with pytest.raises(ValueError):
        replace(_args(), ref_at_liq=2**64)


# --- signature round-trip ------------------------------------------------------------------------


def test_signature_verifies_against_the_exact_message_and_breaks_on_mutation():
    kp = Keypair()
    msg = encode_message(_pk(1), CLUSTER_DEVNET, _args())
    sig = kp.sign_message(msg)
    assert sig.verify(kp.pubkey(), msg)
    mutated = bytearray(msg)
    mutated[0] ^= 1
    assert not sig.verify(kp.pubkey(), bytes(mutated))


# --- price conversion ------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("price_usd", "expected_fp"),
    [
        (330.1171592874945, 33_011_715_929),
        (1.0, 100_000_000),
        (0.005, 500_000),
    ],
)
def test_price_to_fp(price_usd, expected_fp):
    assert price_to_fp(price_usd) == expected_fp


# --- reference lookups (synthetic recordings, isolated from the live aaplx.jsonl fixture) --------


def _write_recording(tmp_path: Path, symbol: str, rows: list[dict]) -> Path:
    p = tmp_path / f"{symbol.lower()}.jsonl"
    import json

    p.write_text("\n".join(json.dumps(r) for r in rows) + "\n")
    return tmp_path


def _row(ts: int, price: float, suffix: str) -> dict:
    return {
        "fetched_at": _iso(ts),
        "price_usd": price,
        "raw_sha256": f"deadbeef{suffix}",
    }


def test_reference_at_picks_the_closest_sample(tmp_path):
    rows = [_row(1_000_000_000, 100.0, "a"), _row(1_000_000_300, 101.0, "b"), _row(1_000_000_600, 102.0, "c")]
    recdir = _write_recording(tmp_path, "AAPLX", rows)
    got = reference_at("AAPLX", target_ts=1_000_000_290, recordings_dir=recdir)
    assert got["price_usd"] == 101.0


def test_reference_at_raises_past_the_staleness_guard(tmp_path):
    rows = [_row(1_000_000_000, 100.0, "a")]
    recdir = _write_recording(tmp_path, "AAPLX", rows)
    with pytest.raises(VerdictEngineError):
        reference_at("AAPLX", target_ts=1_000_005_000, max_gap_secs=900, recordings_dir=recdir)


def test_reference_after_returns_none_when_the_recorder_has_not_caught_up(tmp_path):
    rows = [_row(1_000_000_000, 100.0, "a")]
    recdir = _write_recording(tmp_path, "AAPLX", rows)
    assert reference_after("AAPLX", min_ts=1_000_010_000, recordings_dir=recdir) is None


def test_reference_after_returns_the_first_sample_at_or_after_the_floor(tmp_path):
    rows = [_row(1_000_000_000, 100.0, "a"), _row(1_000_003_600, 101.0, "b"), _row(1_000_003_900, 102.0, "c")]
    recdir = _write_recording(tmp_path, "AAPLX", rows)
    got = reference_after("AAPLX", min_ts=1_000_003_600, recordings_dir=recdir)
    assert got["price_usd"] == 101.0


# --- evidence hash --------------------------------------------------------------------------


def test_evidence_hash_changes_with_either_sample():
    sample_a = {"fetched_at": "x", "price_usd": 1.0, "raw_sha256": "a"}
    sample_b = {"fetched_at": "y", "price_usd": 2.0, "raw_sha256": "b"}
    base = compute_evidence_hash(_pk(1), _pk(2), sample_a, sample_b)
    assert len(base) == 32
    assert compute_evidence_hash(_pk(1), _pk(2), sample_b, sample_b) != base
    assert compute_evidence_hash(_pk(1), _pk(2), sample_a, sample_a) != base
    assert compute_evidence_hash(_pk(3), _pk(2), sample_a, sample_b) != base


# --- sign_verdict end to end, against synthetic recordings ------------------------------------


def test_sign_verdict_end_to_end(tmp_path):
    liquidated_at = 1_000_000_000
    rows = [
        _row(liquidated_at - 60, 100.0, "a"),  # ref_at_liq
        _row(liquidated_at + MIN_AFTER_WAIT_SECS, 60.0, "b"),  # ref_after, right at the floor
    ]
    recdir = _write_recording(tmp_path, "AAPLX", rows)
    kp = Keypair()
    sv = sign_verdict(
        liquidation_record=_pk(7),
        borrower=_pk(8),
        liquidated_at=liquidated_at,
        symbol="AAPLX",
        oracle_key=kp,
        now=liquidated_at + MIN_AFTER_WAIT_SECS,
        recordings_dir=recdir,
    )
    assert sv.args.ref_at_liq == price_to_fp(100.0)
    assert sv.args.ref_after == price_to_fp(60.0)
    assert sv.args.after_ts == liquidated_at + MIN_AFTER_WAIT_SECS
    assert sv.oracle_pubkey == kp.pubkey()
    sig = kp.sign_message(sv.message)
    assert bytes(sig) == sv.signature
    assert sig.verify(kp.pubkey(), sv.message)


def test_sign_verdict_refuses_before_the_wait_floor(tmp_path):
    liquidated_at = 1_000_000_000
    recdir = _write_recording(tmp_path, "AAPLX", [_row(liquidated_at, 100.0, "a")])
    with pytest.raises(VerdictEngineError):
        sign_verdict(
            liquidation_record=_pk(7),
            borrower=_pk(8),
            liquidated_at=liquidated_at,
            symbol="AAPLX",
            oracle_key=Keypair(),
            now=liquidated_at + 10,
            recordings_dir=recdir,
        )


def test_sign_verdict_refuses_when_no_sample_exists_past_the_floor(tmp_path):
    liquidated_at = 1_000_000_000
    recdir = _write_recording(tmp_path, "AAPLX", [_row(liquidated_at, 100.0, "a")])
    with pytest.raises(VerdictEngineError):
        sign_verdict(
            liquidation_record=_pk(7),
            borrower=_pk(8),
            liquidated_at=liquidated_at,
            symbol="AAPLX",
            oracle_key=Keypair(),
            now=liquidated_at + MIN_AFTER_WAIT_SECS,
            recordings_dir=recdir,
        )


def test_sign_verdict_refuses_a_ref_after_sample_past_the_window(tmp_path):
    liquidated_at = 1_000_000_000
    rows = [
        _row(liquidated_at, 100.0, "a"),
        _row(liquidated_at + MAX_AFTER_WAIT_SECS + 1, 60.0, "b"),
    ]
    recdir = _write_recording(tmp_path, "AAPLX", rows)
    with pytest.raises(VerdictEngineError):
        sign_verdict(
            liquidation_record=_pk(7),
            borrower=_pk(8),
            liquidated_at=liquidated_at,
            symbol="AAPLX",
            oracle_key=Keypair(),
            now=liquidated_at + MAX_AFTER_WAIT_SECS + 1,
            recordings_dir=recdir,
        )
