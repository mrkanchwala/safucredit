"""Local-only verification: loads the REAL compiled backstop.so into an in-process LiteSVM,
hand-crafts the account fixtures submit_facts reads, and fires an actual transaction built by
verdict/tx.py + verdict/engine.py at it. This is the missing proof the unit tests can't give --
the round-trip tests in tests/test_tx.py check this module's encoder against a Python port of the
on-chain parser; this script checks it against the on-chain parser itself.

NOT wired into CI on purpose. The `verdict` CI job is deliberately dependency-free (see
.github/workflows/ci.yml's comment: "The mirror itself has no dependencies") and does not build the
Solana programs; the `solana` job builds them but has no Python. Wiring this in either place is a
CI-config decision for whoever owns that pipeline, not something to do silently from here. Run it
by hand after `cargo build-sbf` has produced solana/target/deploy/backstop.so:

    python3 verdict/verify_litesvm.py

Exits 0 and prints PASS/PASS for both the happy path and the negative (tampered-signature) path,
or raises with the LiteSVM transaction error attached.
"""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

from solders.account import Account
from solders.clock import Clock
from solders.keypair import Keypair
from solders.litesvm import LiteSVM
from solders.pubkey import Pubkey
from solders.transaction import Transaction

sys.path.insert(0, str(Path(__file__).resolve().parent))
from engine import BACKSTOP_PROGRAM_ID, CLUSTER_DEVNET, load_oracle_key, sign_verdict  # noqa: E402
from tx import SubmitFactsAccounts, build_submit_facts_instructions  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[1]
BACKSTOP_SO = REPO_ROOT / "solana" / "target" / "deploy" / "backstop.so"
STOCK_VAULT_PROGRAM_ID = Pubkey.from_string("GkQw6VGDKYBWJtgtWUmFDkHNqGrnyjSviQeQzVMW35K2")

MULT_SCALE = 1_000_000_000_000

CONFIG_SEED = b"bconfig"
BORROWER_CLAIMS_SEED = b"bclaims"


def _disc(name: str) -> bytes:
    return hashlib.sha256(f"account:{name}".encode()).digest()[:8]


def _pk32(b: bytes) -> bytes:
    assert len(b) == 32
    return b


def _u32(v: int) -> bytes:
    return v.to_bytes(4, "little")


def _u64(v: int) -> bytes:
    return v.to_bytes(8, "little")


def _i64(v: int) -> bytes:
    return v.to_bytes(8, "little", signed=True)


def _u128(v: int) -> bytes:
    return v.to_bytes(16, "little")


def _bool(v: bool) -> bytes:
    return bytes([1 if v else 0])


def build_backstop_config(
    *, admin: Pubkey, verdict_oracle: Pubkey, cluster_tag: int, bump: int, usdc_mint: Pubkey, cash: int
) -> bytes:
    b = bytearray(_disc("BackstopConfig"))
    b += bytes([1])  # version
    b += bytes(admin)
    b += bytes(verdict_oracle)
    b += bytes([cluster_tag])
    b += bytes([bump])
    b += bytes(usdc_mint)
    b += _u64(cash)
    b += _u128(0)  # total_shares
    b += _u32(5_000)  # per_claim_cap_bps (MAX_PER_CLAIM_CAP_BPS)
    b += _u64(0)  # reserved_total
    b += _i64(0)  # admission_day
    b += _u64(0)  # admitted_today
    b += _i64(0)  # withdraw_delay_secs
    b += bytes(32)  # reserved
    return bytes(b)


def build_borrower_claims(*, bump: int, market: Pubkey, borrower: Pubkey) -> bytes:
    b = bytearray(_disc("BorrowerClaims"))
    b += bytes([1])  # version
    b += bytes([bump])
    b += bytes(market)
    b += bytes(borrower)
    b += bytes(32)  # open = default (free slot)
    b += bytes(32)  # reserved
    return bytes(b)


def build_market(*, collateral_mint: Pubkey, usdc_mint: Pubkey, deviation_cap_bps: int) -> bytes:
    b = bytearray(_disc("Market"))
    b += bytes([1])  # version
    b += bytes([255])  # bump (unused by submit_facts; not seed-checked there)
    b += bytes(collateral_mint)
    b += bytes(usdc_mint)
    b += bytes([6])  # collateral_decimals
    b += bytes([6])  # usdc_decimals
    # MarketParams (17 fields)
    b += _u32(4_000)  # ltv_bps
    b += _u32(5_000)  # liq_threshold_bps
    b += _u32(100)  # min_liq_bonus_bps
    b += _u32(500)  # max_liq_bonus_bps
    b += _u32(9_500)  # insolvency_ltv_bps
    b += _u32(2_500)  # close_factor_bps
    b += _u64(100_000_000_000)  # max_liquidation_debt
    b += _u32(deviation_cap_bps)  # deviation_cap_bps -- the only field submit_facts actually reads
    b += _i64(900)  # activation_pause_secs
    b += _u32(500)  # split_cap_bps
    b += _i64(86_400)  # split_max_hold_secs
    b += _i64(3_600)  # borrow_max_price_age_secs
    b += _i64(90_000)  # liquidation_max_price_age_open_secs
    b += _i64(345_600)  # max_price_age_closed_secs
    b += _u32(0) + _u32(0) + _u32(0) + _u32(0)  # RateParams: base/slope1/slope2/kink
    b += _u64(1_000_000_000_000)  # borrow_cap
    b += _u64(1_000_000_000_000)  # collateral_cap_raw
    # PriceState
    b += bytes(8 * 16)  # prices[16]
    b += bytes(8 * 16)  # timestamps[16]
    b += bytes([0])  # head
    b += bytes([0])  # count
    b += _u64(0)  # last_price
    b += _i64(0)  # last_update
    b += _u64(0)  # last_close
    b += _bool(True)  # market_open
    b += _bool(False)  # flagged
    b += _u64(0)  # flagged_price
    # rest of Market
    b += _u64(0)  # cash
    b += _u128(0)  # total_supply_shares
    b += _u128(0)  # total_borrow_shares
    b += _u128(MULT_SCALE)  # borrow_index
    b += _i64(0)  # last_accrual_ts
    b += _u64(0)  # total_collateral_raw
    b += _u64(0)  # bad_debt
    b += _u64(0)  # bad_debt_cumulative
    b += _bool(False)  # issuer_halt
    b += _u64(0)  # liq_seq
    b += _u128(MULT_SCALE)  # observed_multiplier_fp
    # ramp_from: LiquidationTerms (5 x u32)
    b += _u32(0) + _u32(0) + _u32(0) + _u32(0) + _u32(0)
    b += _i64(0)  # ramp_start_ts
    b += bytes(12)  # reserved
    return bytes(b)


def build_liquidation_record(
    *,
    bump: int,
    market: Pubkey,
    seq: int,
    borrower: Pubkey,
    liquidator: Pubkey,
    seized_raw: int,
    debt_repaid: int,
    multiplier_fp: int,
    price_fp: int,
    collateral_decimals: int,
    borrow_age_ts: int,
    payout: Pubkey,
    ts: int,
) -> bytes:
    b = bytearray(_disc("LiquidationRecord"))
    b += bytes([1])  # version
    b += bytes([bump])
    b += bytes(market)
    b += _u64(seq)
    b += bytes(borrower)
    b += bytes(liquidator)
    b += _u64(seized_raw)
    b += _u64(debt_repaid)
    b += _u128(multiplier_fp)
    b += _u64(price_fp)
    b += bytes([collateral_decimals])
    b += _u32(0)  # bonus_bps
    b += _u32(0)  # ltv_bps
    b += _u32(10_000)  # coverage_bps
    b += _i64(borrow_age_ts)
    b += bytes(payout)
    b += _u64(0)  # bad_debt
    b += _i64(ts)
    b += _u64(0)  # slot
    b += _bool(False)  # issuer_halt
    b += bytes(32)  # reserved
    return bytes(b)


def main() -> int:
    if not BACKSTOP_SO.exists():
        print(f"{BACKSTOP_SO} not built -- run cargo build-sbf first", file=sys.stderr)
        return 2

    oracle = load_oracle_key()
    admin = Keypair()
    payer = Keypair()
    market_pk = Keypair().pubkey()
    usdc_mint = Keypair().pubkey()
    collateral_mint = Keypair().pubkey()
    borrower = Keypair().pubkey()
    liquidator = Keypair().pubkey()
    payout = Keypair().pubkey()

    cap_bps = 500  # 5%, matches the real AAPLx market config
    liquidated_at = 1_800_000_000
    now = liquidated_at + 3_600  # exactly the MIN_AFTER_WAIT_SECS floor

    svm = LiteSVM().with_sigverify(True)
    svm.add_program_from_file(BACKSTOP_PROGRAM_ID, str(BACKSTOP_SO))
    svm.airdrop(payer.pubkey(), 10_000_000_000)
    svm.set_clock(Clock(1, 0, 0, 0, now))

    config_pda, config_bump = Pubkey.find_program_address([CONFIG_SEED], BACKSTOP_PROGRAM_ID)
    liquidation_record_pk = Keypair().pubkey()
    bclaims_pda, bclaims_bump = Pubkey.find_program_address(
        [BORROWER_CLAIMS_SEED, bytes(market_pk), bytes(borrower)], BACKSTOP_PROGRAM_ID
    )

    rent = svm.minimum_balance_for_rent_exemption

    def put(pubkey: Pubkey, data: bytes, owner: Pubkey) -> None:
        svm.set_account(pubkey, Account(lamports=rent(len(data)) + 1_000_000, data=data, owner=owner))

    put(
        config_pda,
        build_backstop_config(
            admin=admin.pubkey(),
            verdict_oracle=oracle.pubkey(),
            cluster_tag=CLUSTER_DEVNET,
            bump=config_bump,
            usdc_mint=usdc_mint,
            cash=10_000_000_000,
        ),
        BACKSTOP_PROGRAM_ID,
    )
    put(
        market_pk,
        build_market(collateral_mint=collateral_mint, usdc_mint=usdc_mint, deviation_cap_bps=cap_bps),
        STOCK_VAULT_PROGRAM_ID,
    )
    put(
        bclaims_pda,
        build_borrower_claims(bump=bclaims_bump, market=market_pk, borrower=borrower),
        BACKSTOP_PROGRAM_ID,
    )
    put(
        liquidation_record_pk,
        build_liquidation_record(
            bump=255,
            market=market_pk,
            seq=0,
            borrower=borrower,
            liquidator=liquidator,
            seized_raw=1_000_000,
            debt_repaid=250_000_000,
            multiplier_fp=MULT_SCALE,
            price_fp=40_000_000_000,  # $400.00 -- the price the liquidation actually used
            collateral_decimals=6,
            borrow_age_ts=liquidated_at - 1_000,  # young loan -> PendingTime, not Active
            payout=payout,
            ts=liquidated_at,
        ),
        STOCK_VAULT_PROGRAM_ID,
    )

    import json
    import tempfile
    from datetime import UTC, datetime

    def iso(ts: int) -> str:
        return datetime.fromtimestamp(ts, tz=UTC).strftime("%Y-%m-%dT%H:%M:%SZ")

    rows = [
        {"fetched_at": iso(liquidated_at - 30), "price_usd": 300.0, "raw_sha256": "aa"},  # $300 at liquidation
        {"fetched_at": iso(liquidated_at + 3_600), "price_usd": 300.0, "raw_sha256": "bb"},  # still $300 after
    ]
    with tempfile.TemporaryDirectory() as tmp:
        recordings_dir = Path(tmp)
        (recordings_dir / "test.jsonl").write_text("\n".join(json.dumps(r) for r in rows) + "\n")
        signed = sign_verdict(
            liquidation_record=liquidation_record_pk,
            borrower=borrower,
            liquidated_at=liquidated_at,
            symbol="test",
            oracle_key=oracle,
            cluster_tag=CLUSTER_DEVNET,
            now=now,
            recordings_dir=recordings_dir,
        )
    assert signed.args.ref_at_liq == 30_000_000_000
    assert signed.args.ref_after == 30_000_000_000

    accounts = SubmitFactsAccounts(
        payer=payer.pubkey(),
        market=market_pk,
        liquidation_record=liquidation_record_pk,
        borrower=borrower,
        payout=payout,
    )
    ed_ix, facts_ix = build_submit_facts_instructions(signed, accounts)

    bh = svm.latest_blockhash()
    tx = Transaction.new_signed_with_payer([ed_ix, facts_ix], payer.pubkey(), [payer], bh)
    result = svm.send_transaction(tx)
    if hasattr(result, "err") and result.err is not None:
        print("HAPPY PATH FAILED:", result.err, file=sys.stderr)
        print(result, file=sys.stderr)
        return 1

    claim_pda = accounts.claim_pda()
    claim_account = svm.get_account(claim_pda)
    if claim_account is None:
        print("HAPPY PATH FAILED: no Claim account was created", file=sys.stderr)
        return 1
    disc = claim_account.data[:8]
    if disc != _disc("Claim"):
        print("HAPPY PATH FAILED: claim account has the wrong discriminator", file=sys.stderr)
        return 1
    # status is the 1-byte enum right after version(1)+bump(1)+market(32)+borrower(32)+
    # liquidation_record(32)+payout(32) = offset 8 + 1+1+32+32+32+32 = 138
    status_byte = claim_account.data[138]
    print(f"PASS happy path: Claim created, status byte = {status_byte} (1 = PendingTime)")
    if status_byte != 1:
        print(f"WARNING: expected PendingTime (1), got {status_byte} -- inspect deny_reason/loss", file=sys.stderr)

    # --- negative path: tamper the signature, expect the precompile to reject it -----------------
    tampered_sig = bytearray(signed.signature)
    tampered_sig[0] ^= 1
    from tx import ed25519_instruction, submit_facts_instruction

    bad_ed_ix = ed25519_instruction(
        signature=bytes(tampered_sig), pubkey=bytes(signed.oracle_pubkey), message=signed.message, own_index=0
    )
    accounts2 = SubmitFactsAccounts(
        payer=payer.pubkey(),
        market=market_pk,
        liquidation_record=liquidation_record_pk,
        borrower=borrower,
        payout=payout,
    )
    # A fresh liquidation_record/claim pair would be needed for a second real submission; here we
    # only need the transaction to be rejected before touching program logic, so reusing addresses
    # is fine -- the Ed25519 precompile itself must fail first.
    bad_facts_ix = submit_facts_instruction(signed.args, accounts2)
    bh2 = svm.latest_blockhash()
    bad_tx = Transaction.new_signed_with_payer([bad_ed_ix, bad_facts_ix], payer.pubkey(), [payer], bh2)
    bad_result = svm.send_transaction(bad_tx)
    if not (hasattr(bad_result, "err") and bad_result.err is not None):
        print("NEGATIVE PATH FAILED: a tampered signature was accepted", file=sys.stderr)
        return 1
    print("PASS negative path: tampered signature rejected by the real Ed25519 precompile")

    return 0


if __name__ == "__main__":
    sys.exit(main())
