"""Tests for verdict/tx.py.

The Ed25519 instruction test is the important one: it re-implements
solana/programs/backstop/src/verdict.rs::check_ed25519_data in Python (same field-by-field checks,
same order) and runs it against tx.py's own ed25519_instruction() output. That proves the two sides
agree on the wire format without needing a running validator -- what it does NOT prove is that the
real Solana runtime's own Ed25519 precompile verifies the signature the same way; that only a
localnet/devnet submission can confirm (not done here -- flagged as the next gap, not silently
assumed).

PDA seeds are pinned against the actual Rust source text below (test_seeds_match_the_rust_source),
so a seed rename in state.rs fails this suite instead of silently deriving the wrong PDA.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from solders.keypair import Keypair
from solders.pubkey import Pubkey

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from engine import BACKSTOP_PROGRAM_ID, CLUSTER_DEVNET, sign_verdict  # noqa: E402
from tx import (  # noqa: E402
    BACKER_SEED,
    BORROWER_CLAIMS_SEED,
    CLAIM_SEED,
    CONFIG_SEED,
    ED25519_PROGRAM_ID,
    INSTRUCTIONS_SYSVAR_ID,
    REVOKE_SEED,
    SUBMIT_FACTS_DISCRIMINATOR,
    SYSTEM_PROGRAM_ID,
    SubmitFactsAccounts,
    build_submit_facts_instructions,
    ed25519_instruction,
    revoked_pda,
    submit_facts_instruction,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
BACKSTOP_STATE_RS = (REPO_ROOT / "solana" / "programs" / "backstop" / "src" / "state.rs").read_text()


def _pk(byte: int) -> Pubkey:
    return Pubkey(bytes([byte] * 32))


# --- Python port of verdict.rs::check_ed25519_data -----------------------------------------------


class _RejectedByOnChainCheck(Exception):
    pass


def _read_u16(data: bytes, at: int) -> int:
    return int.from_bytes(data[at : at + 2], "little")


def python_check_ed25519_data(data: bytes, own_index: int, expected_signer: bytes, expected_message: bytes) -> None:
    """Field-for-field port of verdict.rs::check_ed25519_data. Raises _RejectedByOnChainCheck with
    the same reason the Rust `require!` would, or returns None on success."""
    data_start = 16
    if len(data) < data_start:
        raise _RejectedByOnChainCheck("MalformedEd25519Instruction: too short")
    if data[0] != 1:
        raise _RejectedByOnChainCheck("WrongSignatureCount")
    if data[1] != 0:
        raise _RejectedByOnChainCheck("MalformedEd25519Instruction: nonzero padding")

    signature_offset = _read_u16(data, 2)
    signature_ix = _read_u16(data, 4)
    pubkey_offset = _read_u16(data, 6)
    pubkey_ix = _read_u16(data, 8)
    message_offset = _read_u16(data, 10)
    message_size = _read_u16(data, 12)
    message_ix = _read_u16(data, 14)

    if not (signature_ix == own_index and pubkey_ix == own_index and message_ix == own_index):
        raise _RejectedByOnChainCheck("OffsetsOutsideEd25519Instruction")

    sig = data[signature_offset : signature_offset + 64]
    if len(sig) != 64:
        raise _RejectedByOnChainCheck("MalformedEd25519Instruction: signature out of bounds")

    pubkey = data[pubkey_offset : pubkey_offset + 32]
    if len(pubkey) != 32:
        raise _RejectedByOnChainCheck("MalformedEd25519Instruction: pubkey out of bounds")
    if pubkey != expected_signer:
        raise _RejectedByOnChainCheck("WrongVerdictSigner")

    if message_size != len(expected_message):
        raise _RejectedByOnChainCheck("VerdictMessageMismatch: size")
    message = data[message_offset : message_offset + message_size]
    if message != expected_message:
        raise _RejectedByOnChainCheck("VerdictMessageMismatch: content")


# --- ed25519_instruction ---------------------------------------------------------------------


def test_ed25519_instruction_round_trips_through_a_python_port_of_check_ed25519_data():
    kp = Keypair()
    msg = b"some verdict message, arbitrary length here"
    sig = bytes(kp.sign_message(msg))
    ix = ed25519_instruction(signature=sig, pubkey=bytes(kp.pubkey()), message=msg, own_index=0)
    assert ix.program_id == ED25519_PROGRAM_ID
    assert ix.accounts == []
    python_check_ed25519_data(ix.data, own_index=0, expected_signer=bytes(kp.pubkey()), expected_message=msg)


def test_ed25519_instruction_rejects_when_own_index_is_wrong():
    kp = Keypair()
    msg = b"x"
    sig = bytes(kp.sign_message(msg))
    ix = ed25519_instruction(signature=sig, pubkey=bytes(kp.pubkey()), message=msg, own_index=2)
    with pytest.raises(_RejectedByOnChainCheck):
        python_check_ed25519_data(ix.data, own_index=0, expected_signer=bytes(kp.pubkey()), expected_message=msg)


def test_ed25519_instruction_rejects_wrong_signer():
    kp = Keypair()
    other = Keypair()
    msg = b"x"
    sig = bytes(kp.sign_message(msg))
    ix = ed25519_instruction(signature=sig, pubkey=bytes(kp.pubkey()), message=msg, own_index=0)
    with pytest.raises(_RejectedByOnChainCheck):
        python_check_ed25519_data(ix.data, own_index=0, expected_signer=bytes(other.pubkey()), expected_message=msg)


def test_ed25519_instruction_rejects_a_tampered_message():
    kp = Keypair()
    msg = b"the real message"
    sig = bytes(kp.sign_message(msg))
    ix = ed25519_instruction(signature=sig, pubkey=bytes(kp.pubkey()), message=msg, own_index=0)
    with pytest.raises(_RejectedByOnChainCheck):
        python_check_ed25519_data(
            ix.data, own_index=0, expected_signer=bytes(kp.pubkey()), expected_message=b"a different message"
        )


def test_ed25519_instruction_rejects_bad_lengths():
    with pytest.raises(ValueError):
        ed25519_instruction(signature=b"\x00" * 63, pubkey=b"\x00" * 32, message=b"x")
    with pytest.raises(ValueError):
        ed25519_instruction(signature=b"\x00" * 64, pubkey=b"\x00" * 31, message=b"x")


# --- seeds, pinned against the actual Rust source -------------------------------------------------


def test_seeds_match_the_rust_source():
    assert 'pub const CONFIG_SEED: &[u8] = b"bconfig";' in BACKSTOP_STATE_RS
    assert 'pub const BACKER_SEED: &[u8] = b"backer";' in BACKSTOP_STATE_RS
    assert 'pub const CLAIM_SEED: &[u8] = b"claim";' in BACKSTOP_STATE_RS
    assert 'pub const BORROWER_CLAIMS_SEED: &[u8] = b"bclaims";' in BACKSTOP_STATE_RS
    assert 'pub const REVOKE_SEED: &[u8] = b"revoke";' in BACKSTOP_STATE_RS
    assert CONFIG_SEED == b"bconfig"
    assert BACKER_SEED == b"backer"
    assert CLAIM_SEED == b"claim"
    assert BORROWER_CLAIMS_SEED == b"bclaims"
    assert REVOKE_SEED == b"revoke"


def test_program_id_matches_declare_id():
    lib_rs = (REPO_ROOT / "solana" / "programs" / "backstop" / "src" / "lib.rs").read_text()
    assert f'declare_id!("{BACKSTOP_PROGRAM_ID}");' in lib_rs


def test_well_known_program_ids():
    assert str(ED25519_PROGRAM_ID) == "Ed25519SigVerify111111111111111111111111111"
    assert str(SYSTEM_PROGRAM_ID) == "11111111111111111111111111111111"
    assert str(INSTRUCTIONS_SYSVAR_ID) == "Sysvar1nstructions1111111111111111111111111"


def test_submit_facts_discriminator_is_the_anchor_global_sighash():
    import hashlib

    assert SUBMIT_FACTS_DISCRIMINATOR == hashlib.sha256(b"global:submit_facts").digest()[:8]
    assert len(SUBMIT_FACTS_DISCRIMINATOR) == 8


# --- submit_facts_instruction account wiring -----------------------------------------------------


def _accounts(**overrides) -> SubmitFactsAccounts:
    base = {
        "payer": _pk(1),
        "market": _pk(2),
        "liquidation_record": _pk(3),
        "borrower": _pk(4),
        "payout": _pk(5),
    }
    base.update(overrides)
    return SubmitFactsAccounts(**base)


def test_submit_facts_instruction_has_twelve_accounts_in_spec_order():
    accs = _accounts()
    from engine import FactsArgs

    args = FactsArgs(
        liquidation_record=accs.liquidation_record,
        borrower=accs.borrower,
        ref_at_liq=1,
        ref_after=1,
        after_ts=1,
        evidence_hash=bytes(32),
        deadline=1,
    )
    ix = submit_facts_instruction(args, accs)
    assert len(ix.accounts) == 12
    assert ix.accounts[0].pubkey == accs.payer
    assert ix.accounts[0].is_signer is True
    assert ix.accounts[1].pubkey == accs.config_pda()
    assert ix.accounts[2].pubkey == accs.market
    assert ix.accounts[3].pubkey == accs.liquidation_record
    assert ix.accounts[4].pubkey == accs.borrower
    assert ix.accounts[5].pubkey == accs.borrower_claims_pda()
    assert ix.accounts[6].pubkey == accs.payer  # existing_claim defaults to payer
    assert ix.accounts[7].pubkey == accs.claim_pda()
    assert ix.accounts[8].pubkey == accs.payout_backer_pda()
    assert ix.accounts[9].pubkey == revoked_pda(args)
    assert ix.accounts[9].is_writable is False
    assert ix.accounts[10].pubkey == INSTRUCTIONS_SYSVAR_ID
    assert ix.accounts[11].pubkey == SYSTEM_PROGRAM_ID
    assert ix.data[:8] == SUBMIT_FACTS_DISCRIMINATOR
    assert len(ix.data) == 8 + 32 + 32 + 8 + 8 + 8 + 32 + 8  # discriminator + FactsArgs Borsh body


def test_existing_claim_override_is_used_instead_of_payer():
    accs = _accounts(existing_claim=_pk(9))
    from engine import FactsArgs

    args = FactsArgs(
        liquidation_record=accs.liquidation_record,
        borrower=accs.borrower,
        ref_at_liq=1,
        ref_after=1,
        after_ts=1,
        evidence_hash=bytes(32),
        deadline=1,
    )
    ix = submit_facts_instruction(args, accs)
    assert ix.accounts[6].pubkey == _pk(9)


def test_pdas_are_deterministic_and_program_scoped():
    accs = _accounts()
    assert accs.config_pda() == accs.config_pda()
    other_accs = _accounts(payout=_pk(6))
    assert accs.payout_backer_pda() != other_accs.payout_backer_pda()


# --- full pipeline: sign_verdict -> build_submit_facts_instructions ------------------------------


def test_build_submit_facts_instructions_end_to_end(tmp_path):
    import json

    liquidated_at = 1_000_000_000

    def row(ts: int, price: float) -> dict:
        from datetime import UTC, datetime

        return {
            "fetched_at": datetime.fromtimestamp(ts, tz=UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "price_usd": price,
            "raw_sha256": "deadbeef",
        }

    rec_dir = tmp_path
    (rec_dir / "aaplx.jsonl").write_text(
        "\n".join(
            json.dumps(r) for r in [row(liquidated_at - 30, 100.0), row(liquidated_at + 3_600, 60.0)]
        )
        + "\n"
    )

    oracle = Keypair()
    signed = sign_verdict(
        liquidation_record=_pk(3),
        borrower=_pk(4),
        liquidated_at=liquidated_at,
        symbol="AAPLX",
        oracle_key=oracle,
        cluster_tag=CLUSTER_DEVNET,
        now=liquidated_at + 3_600,
        recordings_dir=rec_dir,
    )
    accs = _accounts(liquidation_record=_pk(3), borrower=_pk(4))
    ed_ix, facts_ix = build_submit_facts_instructions(signed, accs)

    assert ed_ix.program_id == ED25519_PROGRAM_ID
    assert facts_ix.program_id == BACKSTOP_PROGRAM_ID
    python_check_ed25519_data(
        ed_ix.data, own_index=0, expected_signer=bytes(oracle.pubkey()), expected_message=signed.message
    )
    assert facts_ix.data[:8] == SUBMIT_FACTS_DISCRIMINATOR


def test_revoked_pda_is_bound_to_both_the_record_and_the_evidence_hash():
    from engine import FactsArgs

    def args(record: Pubkey, evidence: bytes) -> FactsArgs:
        return FactsArgs(
            liquidation_record=record,
            borrower=_pk(2),
            ref_at_liq=1,
            ref_after=1,
            after_ts=1,
            evidence_hash=evidence,
            deadline=1,
        )

    base = revoked_pda(args(_pk(1), bytes(32)))
    expected = Pubkey.find_program_address([REVOKE_SEED, bytes(_pk(1)), bytes(32)], BACKSTOP_PROGRAM_ID)[0]
    assert base == expected
    assert revoked_pda(args(_pk(3), bytes(32))) != base
    assert revoked_pda(args(_pk(1), b"\x01" + bytes(31))) != base
