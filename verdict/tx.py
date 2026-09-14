"""Builds the two Solana instructions a signed verdict needs to actually reach the chain:
the native Ed25519 precompile instruction, and the backstop program's `submit_facts` call that
must immediately follow it (verdict.rs::verify_preceding_ed25519 requires exactly that order).

This module does not send anything -- no RPC client, no blockhash, no fee payer signing. It hands
back two `solders.instruction.Instruction`s; a caller (the scenario runner, not built yet) wraps
them in a transaction with a recent blockhash, signs with the fee payer, and sends.

Byte layout for the Ed25519 instruction is ported from solana-ed25519-program 3.0.0's
Ed25519SignatureOffsets format, cross-checked against exactly what
solana/programs/backstop/src/verdict.rs::check_ed25519_data parses (see
tests/test_tx.py::test_ed25519_instruction_round_trips_through_a_python_port_of_check_ed25519_data,
which re-implements that Rust function and runs it against this module's own output).
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass

from engine import BACKSTOP_PROGRAM_ID, FactsArgs, SignedVerdict
from solders.instruction import AccountMeta, Instruction
from solders.pubkey import Pubkey
from solders.system_program import ID as SYSTEM_PROGRAM_ID
from solders.sysvar import INSTRUCTIONS as INSTRUCTIONS_SYSVAR_ID

# Seeds below are copied from solana/programs/backstop/src/state.rs, not imported -- there is no
# Python binding to the Rust crate. tests/test_tx.py pins each one against the source file's own
# text so a seed rename there fails this suite loudly instead of silently deriving wrong PDAs.

ED25519_PROGRAM_ID = Pubkey.from_string("Ed25519SigVerify111111111111111111111111111")

CONFIG_SEED = b"bconfig"
BACKER_SEED = b"backer"
CLAIM_SEED = b"claim"
REVOKE_SEED = b"revoke"
BORROWER_CLAIMS_SEED = b"bclaims"

SUBMIT_FACTS_DISCRIMINATOR = hashlib.sha256(b"global:submit_facts").digest()[:8]

# Ed25519SignatureOffsets wire layout (solana_program::ed25519_program).
_SIGNATURE_LEN = 64
_PUBKEY_LEN = 32
_HEADER_LEN = 2  # [num_signatures, padding]
_OFFSETS_LEN = 14  # 7 x u16 LE
_DATA_START = _HEADER_LEN + _OFFSETS_LEN  # 16


def ed25519_instruction(*, signature: bytes, pubkey: bytes, message: bytes, own_index: int = 0) -> Instruction:
    """One-signature Ed25519 precompile instruction, laid out so verify_preceding_ed25519 accepts
    it when it directly precedes the submit_facts instruction at position own_index + 1.

    own_index must be this instruction's own top-level index in the final transaction -- 0 when
    it is the transaction's first instruction (the default, and the shape this module targets).
    If anything is prepended (e.g. a compute-budget instruction), pass the shifted index; forgetting
    to would build a payload the on-chain check rejects with OffsetsOutsideEd25519Instruction.
    """
    if len(signature) != _SIGNATURE_LEN:
        raise ValueError(f"signature must be {_SIGNATURE_LEN} bytes, got {len(signature)}")
    if len(pubkey) != _PUBKEY_LEN:
        raise ValueError(f"pubkey must be {_PUBKEY_LEN} bytes, got {len(pubkey)}")
    if not (0 <= own_index < 0xFFFF):
        raise ValueError("own_index must fit a u16 and not be the 0xFFFF sentinel")

    sig_offset = _DATA_START
    pubkey_offset = sig_offset + _SIGNATURE_LEN
    message_offset = pubkey_offset + _PUBKEY_LEN

    data = bytearray()
    data += bytes([1, 0])  # one signature, zero padding
    for value in (
        sig_offset,
        own_index,
        pubkey_offset,
        own_index,
        message_offset,
        len(message),
        own_index,
    ):
        data += int(value).to_bytes(2, "little")
    data += signature
    data += pubkey
    data += message
    assert len(data) == _DATA_START + _SIGNATURE_LEN + _PUBKEY_LEN + len(message)

    return Instruction(program_id=ED25519_PROGRAM_ID, data=bytes(data), accounts=[])


def _borsh_facts_args(args: FactsArgs) -> bytes:
    """Borsh encoding of FactsArgs, field order and width matching the Rust struct exactly --
    the same primitives encode_message uses, without the signing domain prefix."""
    b = bytearray()
    b += bytes(args.liquidation_record)
    b += bytes(args.borrower)
    b += args.ref_at_liq.to_bytes(8, "little")
    b += args.ref_after.to_bytes(8, "little")
    b += args.after_ts.to_bytes(8, "little", signed=True)
    b += args.evidence_hash
    b += args.deadline.to_bytes(8, "little", signed=True)
    return bytes(b)


@dataclass(frozen=True)
class SubmitFactsAccounts:
    """Resolved account list for one submit_facts call. market, liquidation_record, borrower and
    payout are supplied by the caller -- they are pre-existing accounts this module has no way to
    look up without an RPC connection, which it deliberately does not hold."""

    payer: Pubkey
    market: Pubkey
    liquidation_record: Pubkey
    borrower: Pubkey
    payout: Pubkey
    program_id: Pubkey = BACKSTOP_PROGRAM_ID
    existing_claim: Pubkey | None = None  # defaults to payer; untouched unless a claim is already open

    def config_pda(self) -> Pubkey:
        return Pubkey.find_program_address([CONFIG_SEED], self.program_id)[0]

    def borrower_claims_pda(self) -> Pubkey:
        return Pubkey.find_program_address(
            [BORROWER_CLAIMS_SEED, bytes(self.market), bytes(self.borrower)], self.program_id
        )[0]

    def claim_pda(self) -> Pubkey:
        return Pubkey.find_program_address([CLAIM_SEED, bytes(self.liquidation_record)], self.program_id)[0]

    def payout_backer_pda(self) -> Pubkey:
        return Pubkey.find_program_address([BACKER_SEED, bytes(self.payout)], self.program_id)[0]


def revoked_pda(args: FactsArgs, program_id: Pubkey = BACKSTOP_PROGRAM_ID) -> Pubkey:
    """Admin revocation marker for this exact (liquidation_record, evidence_hash). Always passed;
    it only exists on-chain if the admin revoked this attestation, and then submit_facts refuses."""
    return Pubkey.find_program_address(
        [REVOKE_SEED, bytes(args.liquidation_record), args.evidence_hash], program_id
    )[0]


def submit_facts_instruction(args: FactsArgs, accounts: SubmitFactsAccounts) -> Instruction:
    """Anchor `submit_facts` call. Account order matches SubmitFacts<'info> in
    solana/programs/backstop/src/lib.rs exactly -- Anchor resolves accounts positionally, so
    reordering this list silently sends data to the wrong slot rather than failing to compile."""
    existing_claim = accounts.existing_claim if accounts.existing_claim is not None else accounts.payer
    metas = [
        AccountMeta(accounts.payer, is_signer=True, is_writable=True),
        AccountMeta(accounts.config_pda(), is_signer=False, is_writable=True),
        AccountMeta(accounts.market, is_signer=False, is_writable=False),
        AccountMeta(accounts.liquidation_record, is_signer=False, is_writable=False),
        AccountMeta(accounts.borrower, is_signer=False, is_writable=False),
        AccountMeta(accounts.borrower_claims_pda(), is_signer=False, is_writable=True),
        AccountMeta(existing_claim, is_signer=False, is_writable=False),
        AccountMeta(accounts.claim_pda(), is_signer=False, is_writable=True),
        AccountMeta(accounts.payout_backer_pda(), is_signer=False, is_writable=False),
        AccountMeta(revoked_pda(args, accounts.program_id), is_signer=False, is_writable=False),
        AccountMeta(INSTRUCTIONS_SYSVAR_ID, is_signer=False, is_writable=False),
        AccountMeta(SYSTEM_PROGRAM_ID, is_signer=False, is_writable=False),
    ]
    data = SUBMIT_FACTS_DISCRIMINATOR + _borsh_facts_args(args)
    return Instruction(program_id=accounts.program_id, data=data, accounts=metas)


def build_submit_facts_instructions(
    signed: SignedVerdict, accounts: SubmitFactsAccounts
) -> tuple[Instruction, Instruction]:
    """The two instructions, in the order they must appear in the transaction: Ed25519 precompile
    first (own_index=0), submit_facts second. A caller that prepends anything else (compute budget,
    etc.) must rebuild the Ed25519 instruction with the shifted own_index -- do not just reorder."""
    ed_ix = ed25519_instruction(
        signature=signed.signature, pubkey=bytes(signed.oracle_pubkey), message=signed.message, own_index=0
    )
    facts_ix = submit_facts_instruction(signed.args, accounts)
    return ed_ix, facts_ix
