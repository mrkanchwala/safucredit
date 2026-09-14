"""Off-chain verdict engine: builds and signs the v2 facts payload that
solana/programs/backstop/src/verdict.rs::submit_facts checks (LOCKED verdict spec,
outputs/2026-09-14_mechanism-review-stocklana-verdict-engine-placement.md).

Scope, deliberately narrow:
  - fetch a reference price at the liquidation and a second one after the wait, from the CMC
    recorder's history (verdict/recordings/<symbol>.jsonl, cmc_recorder.py)
  - hash the evidence, sign FactsArgs with the devnet throwaway Ed25519 oracle key
It does NOT evaluate wrongfulness -- the LOCKED spec puts checks (a)/(b) and the loss math
on-chain, computed by the program from these facts. It does NOT submit a transaction; wiring the
Ed25519 precompile instruction + a submit_facts call is the scenario runner's job, not built yet.

encode_message is a byte-exact port of verdict.rs::encode_message. Any change to that Rust
function must be mirrored here and reverified -- see tests/test_engine.py, which ports verdict.rs's
own #[cfg(test)] vectors.
"""

from __future__ import annotations

import hashlib
import json
import sys
from dataclasses import dataclass
from datetime import UTC, datetime
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path

from solders.keypair import Keypair
from solders.pubkey import Pubkey

sys.path.insert(0, str(Path(__file__).resolve().parent))
from safu_math import (
    PRICE_DECIMALS,  # noqa: E402  8 decimals, same fixed-point scale as the program
)

DOMAIN = b"SAFU_STOCKLANA_V2"
assert len(DOMAIN) == 17
MESSAGE_LEN = 178
MAX_VERDICT_TTL_SECS = 86_400
MIN_AFTER_WAIT_SECS = 3_600
MAX_AFTER_WAIT_SECS = 4 * 86_400

CLUSTER_LOCALNET = 0
CLUSTER_DEVNET = 1
CLUSTER_MAINNET = 2

BACKSTOP_PROGRAM_ID = Pubkey.from_string("H1hApKnNkYPsqQ9WVZGzqZQZYirxCLpXDj2uEmzMK3YV")

RECORDINGS_DIR = Path(__file__).resolve().parent / "recordings"
DEFAULT_KEY_PATH = Path(__file__).resolve().parent / "keys" / "oracle-devnet-keypair.json"


class VerdictEngineError(Exception):
    """Any condition where the engine correctly declines to produce a payload."""


@dataclass(frozen=True)
class FactsArgs:
    """Mirrors backstop::verdict::FactsArgs. Field order and widths are load-bearing -- they are
    what encode_message serializes."""

    liquidation_record: Pubkey
    borrower: Pubkey
    ref_at_liq: int  # u64, USD per whole share, 8 decimals
    ref_after: int  # u64, same scale
    after_ts: int  # i64, unix seconds the second sample was taken
    evidence_hash: bytes  # 32 bytes, sha256
    deadline: int  # i64, unix seconds; submission must land at or before this

    def __post_init__(self) -> None:
        if not (0 <= self.ref_at_liq < 2**64):
            raise ValueError("ref_at_liq out of u64 range")
        if not (0 <= self.ref_after < 2**64):
            raise ValueError("ref_after out of u64 range")
        if len(self.evidence_hash) != 32:
            raise ValueError("evidence_hash must be exactly 32 bytes")


def encode_message(program_id: Pubkey, cluster_tag: int, args: FactsArgs) -> bytes:
    """Byte-exact port of verdict.rs::encode_message. Do not reorder or resize any field --
    doing so silently breaks every signature the oracle has already produced against the old
    layout, since DOMAIN has no version bump built in for a field-shape change."""
    if not (0 <= cluster_tag <= 255):
        raise ValueError("cluster_tag must fit a u8")
    m = bytearray()
    m += DOMAIN
    m += bytes(program_id)
    m += bytes([cluster_tag])
    m += bytes(args.liquidation_record)
    m += bytes(args.borrower)
    m += args.ref_at_liq.to_bytes(8, "little")
    m += args.ref_after.to_bytes(8, "little")
    m += args.after_ts.to_bytes(8, "little", signed=True)
    m += args.evidence_hash
    m += args.deadline.to_bytes(8, "little", signed=True)
    assert len(m) == MESSAGE_LEN, f"expected {MESSAGE_LEN} bytes, got {len(m)}"
    return bytes(m)


# --- CMC reference lookups (from verdict/recordings/<symbol>.jsonl, written by cmc_recorder.py) --


def _parse_ts(iso: str) -> int:
    return int(datetime.strptime(iso, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC).timestamp())


def _load_samples(symbol: str, recordings_dir: Path = RECORDINGS_DIR) -> list[dict]:
    path = recordings_dir / f"{symbol.lower()}.jsonl"
    if not path.exists():
        raise VerdictEngineError(f"no recordings for {symbol}: {path} does not exist")
    samples = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    if not samples:
        raise VerdictEngineError(f"{path} has no recorded samples")
    return samples


def price_to_fp(price_usd: float) -> int:
    """USD (float, as CMC returns it) -> fixed point at PRICE_DECIMALS, matching safu_math /
    the on-chain price scale. Goes through Decimal(str(...)) to avoid binary-float rounding
    landing on the wrong integer at the boundary."""
    d = Decimal(str(price_usd))
    return int((d * (10**PRICE_DECIMALS)).to_integral_value(rounding=ROUND_HALF_UP))


def reference_at(symbol: str, target_ts: int, max_gap_secs: int = 900, recordings_dir: Path = RECORDINGS_DIR) -> dict:
    """Closest recorded sample to target_ts. Raises if the closest one is more than max_gap_secs
    away (default 900s = 3x the recorder's 5-minute cron cadence, a deliberate margin) -- a wider
    gap means the recorder was not running at the relevant time, and that must fail loudly rather
    than sign a stale price as if it were current."""
    samples = _load_samples(symbol, recordings_dir)
    best = min(samples, key=lambda s: abs(_parse_ts(s["fetched_at"]) - target_ts))
    gap = abs(_parse_ts(best["fetched_at"]) - target_ts)
    if gap > max_gap_secs:
        raise VerdictEngineError(
            f"closest {symbol} sample is {gap}s from target_ts={target_ts}, over the "
            f"{max_gap_secs}s staleness guard"
        )
    return best


def reference_after(symbol: str, min_ts: int, recordings_dir: Path = RECORDINGS_DIR) -> dict | None:
    """First recorded sample at or after min_ts (the wait floor). Returns None -- not an error --
    when the recorder has not caught up yet; callers should treat that as 'not ready' and retry
    later rather than fabricate an early sample to avoid waiting."""
    samples = _load_samples(symbol, recordings_dir)
    candidates = [s for s in samples if _parse_ts(s["fetched_at"]) >= min_ts]
    if not candidates:
        return None
    return min(candidates, key=lambda s: _parse_ts(s["fetched_at"]))


# --- evidence hash --------------------------------------------------------------------------


def compute_evidence_hash(
    liquidation_record: Pubkey, borrower: Pubkey, sample_at_liq: dict, sample_after: dict
) -> bytes:
    """sha256 over the two recorded samples plus the claim identity, so the attestation is
    reproducible by anyone holding verdict/recordings/. Each sample's raw_sha256 (of the archived
    CMC response bytes, written by cmc_recorder.py) is what actually anchors this to real data --
    binding it here is the whole reason that field exists."""
    evidence = {
        "liquidation_record": str(liquidation_record),
        "borrower": str(borrower),
        "sample_at_liq": {k: sample_at_liq[k] for k in ("fetched_at", "price_usd", "raw_sha256")},
        "sample_after": {k: sample_after[k] for k in ("fetched_at", "price_usd", "raw_sha256")},
    }
    payload = json.dumps(evidence, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).digest()


# --- oracle key -------------------------------------------------------------------------------


def load_oracle_key(path: Path = DEFAULT_KEY_PATH) -> Keypair:
    if not path.exists():
        raise VerdictEngineError(f"no oracle key at {path} -- run verdict/generate_oracle_key.py first")
    raw = json.loads(path.read_text())
    return Keypair.from_bytes(bytes(raw))


@dataclass(frozen=True)
class SignedVerdict:
    args: FactsArgs
    message: bytes
    signature: bytes
    oracle_pubkey: Pubkey
    cluster_tag: int
    program_id: Pubkey


def sign_verdict(
    *,
    liquidation_record: Pubkey,
    borrower: Pubkey,
    liquidated_at: int,
    symbol: str,
    oracle_key: Keypair,
    program_id: Pubkey = BACKSTOP_PROGRAM_ID,
    cluster_tag: int = CLUSTER_DEVNET,
    ttl_secs: int = 3_600,
    now: int | None = None,
    recordings_dir: Path = RECORDINGS_DIR,
) -> SignedVerdict:
    """Builds and signs one FactsArgs payload for a liquidation.

    Does not evaluate wrongfulness -- see module docstring. Raises VerdictEngineError if the
    MIN_AFTER_WAIT_SECS floor has not elapsed yet, if no ref_after sample has been recorded past
    it (retry later), or if the first available sample past the floor is already outside
    MAX_AFTER_WAIT_SECS (escalate rather than submit a payload the program will reject).
    """
    now = now if now is not None else int(datetime.now(UTC).timestamp())
    min_after_ts = liquidated_at + MIN_AFTER_WAIT_SECS
    max_after_ts = liquidated_at + MAX_AFTER_WAIT_SECS
    if now < min_after_ts:
        raise VerdictEngineError(
            f"only {now - liquidated_at}s since the liquidation; the on-chain gate needs at "
            f"least {MIN_AFTER_WAIT_SECS}s before ref_after can be sampled"
        )

    sample_at_liq = reference_at(symbol, liquidated_at, recordings_dir=recordings_dir)
    sample_after = reference_after(symbol, min_after_ts, recordings_dir=recordings_dir)
    if sample_after is None:
        raise VerdictEngineError(
            f"no {symbol} sample recorded at or after {min_after_ts} yet -- retry once the cron "
            "recorder has caught up"
        )
    after_ts = _parse_ts(sample_after["fetched_at"])
    if after_ts > max_after_ts:
        raise VerdictEngineError(
            f"first available {symbol} sample past the wait floor is at {after_ts}, beyond the "
            f"{MAX_AFTER_WAIT_SECS}s window -- the claim window may be closing, escalate rather "
            "than submit a payload the program will reject"
        )

    args = FactsArgs(
        liquidation_record=liquidation_record,
        borrower=borrower,
        ref_at_liq=price_to_fp(sample_at_liq["price_usd"]),
        ref_after=price_to_fp(sample_after["price_usd"]),
        after_ts=after_ts,
        evidence_hash=compute_evidence_hash(liquidation_record, borrower, sample_at_liq, sample_after),
        deadline=now + min(ttl_secs, MAX_VERDICT_TTL_SECS),
    )
    message = encode_message(program_id, cluster_tag, args)
    signature = bytes(oracle_key.sign_message(message))
    return SignedVerdict(
        args=args,
        message=message,
        signature=signature,
        oracle_pubkey=oracle_key.pubkey(),
        cluster_tag=cluster_tag,
        program_id=program_id,
    )
