"""Stocklana demo runner: real programs, real validator clock, real transactions.

    python3 scripts/demo.py wrongful     # bad data drifts inside the cap -> pool liquidates -> verdict -> paid back 1:1
    python3 scripts/demo.py real-crash   # a genuine fall -> pool liquidates -> verdict -> denied (MOVE_HELD), nothing paid
    python3 scripts/demo.py split        # scheduled 4:1 split + early post-split price -> held, never liquidated

Each run starts a fresh local validator with both compiled programs (see localnet.py), builds the market from
scratch, runs one scenario end to end and stops the validator (`--keep` leaves it running).

Disclosed demo settings (on-chain, readable): devnet cluster tag, claim timings shortened with `set_claim_timing`
to seconds (gate, cooldown, stream, minimum wait), a mock AAPLx Token-2022 mint, and a SAFU-controlled price feed.
The reference price comes from real recorded CoinMarketCap AAPLx samples, re-timestamped onto the run's clock;
any price that is not a real recorded sample is written with `raw_sha256 = "synthetic:..."` so it can never pass
as real data. The verdict is signed by the unchanged verdict engine (verdict/engine.py) and submitted with the
layout proven against the compiled program (verdict/tx.py).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "scripts"))
sys.path.insert(0, str(REPO / "verdict"))

import localnet  # noqa: E402
import tokens  # noqa: E402
from engine import CLUSTER_DEVNET, load_oracle_key, sign_verdict  # noqa: E402
from idl_client import Program, pda  # noqa: E402
from rpc import Rpc, RpcError  # noqa: E402
from solders.keypair import Keypair  # noqa: E402
from solders.pubkey import Pubkey  # noqa: E402
from tx import SubmitFactsAccounts, build_submit_facts_instructions  # noqa: E402

SYSTEM = Pubkey.from_string("11111111111111111111111111111111")
LOADER_V3 = Pubkey.from_string("BPFLoaderUpgradeab1e11111111111111111111111")
USDC = 1_000_000
ONE_SHARE = 10**tokens.AAPLX_DECIMALS
PRICE_SCALE = 10**8
LIVE_MULTIPLIER = 1.0026642  # live AAPLx mint, read 2026-09-13

# gate, cooldown, stream, inactivity, minimum wait -- seconds, disclosed demo values (devnet floors allow them).
DEMO_TIMING = {"gate_secs": 240, "cooldown_secs": 20, "stream_secs": 40, "inactivity_secs": 3_600, "min_after_wait_secs": 20}

MARKET_PARAMS = {  # outputs/2026-09-14_stocklana-market-config-research.md §2b (AAPLx), demo caps
    "ltv_bps": 4_000, "liq_threshold_bps": 5_000, "min_liq_bonus_bps": 100, "max_liq_bonus_bps": 500,
    "insolvency_ltv_bps": 9_500, "close_factor_bps": 2_500, "max_liquidation_debt": 100_000 * USDC,
    "deviation_cap_bps": 500, "activation_pause_secs": 900, "split_cap_bps": 500, "split_max_hold_secs": 86_400,
    "borrow_max_price_age_secs": 3_600, "liquidation_max_price_age_open_secs": 90_000,
    "max_price_age_closed_secs": 345_600,
    "rate": {"base_bps": 100, "slope1_bps": 500, "slope2_bps": 6_000, "kink_bps": 9_000},
    "borrow_cap": 500_000 * USDC, "collateral_cap_raw": 2_000 * ONE_SHARE, "backer_interest_share_bps": 1_500,
}


def say(msg: str) -> None:
    print(f"[{datetime.now(UTC).strftime('%H:%M:%S')}] {msg}", flush=True)


def fmt_usd(fp8: int) -> str:
    return f"${fp8 / PRICE_SCALE:,.2f}"


# --- reference recorder ----------------------------------------------------------------------------------------


class Recorder:
    """Stands in for the CMC cron recorder on the run's clock. Writes the engine's own recording format."""

    def __init__(self, out_dir: Path, source: Path):
        self.dir = out_dir
        self.dir.mkdir(parents=True, exist_ok=True)
        self.path = self.dir / "aaplx.jsonl"
        self.path.write_text("")
        rows = [json.loads(line) for line in source.read_text().splitlines() if line.strip()]
        if not rows:
            raise SystemExit(f"no recorded AAPLx samples in {source}")
        self.real = rows
        self.real_price = rows[-1]["price_usd"]

    def sample(self, price_usd: float | None = None) -> float:
        """Records one sample stamped now. No price -> the latest real recorded sample, replayed."""
        base = self.real[-1]
        row = dict(base)
        row["fetched_at"] = datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        row["replay_of"] = base["fetched_at"]
        if price_usd is None:
            price_usd = base["price_usd"]
        else:
            row["raw_sha256"] = "synthetic:" + hashlib.sha256(f"{row['fetched_at']}:{price_usd}".encode()).hexdigest()
            row["synthetic"] = True
        row["price_usd"] = price_usd
        with self.path.open("a") as f:
            f.write(json.dumps(row, sort_keys=True) + "\n")
        return price_usd


# --- chain helpers ---------------------------------------------------------------------------------------------


@dataclass
class Keys:
    admin: Keypair
    admin_path: Path
    feed: Keypair
    issuer: Keypair
    issuer_path: Path
    co_signer: Keypair
    alice: Keypair
    bob: Keypair
    backer: Keypair
    crank: Keypair


class Demo:
    def __init__(self, rpc: Rpc):
        self.rpc = rpc
        self.vault = Program("stock_vault")
        self.backstop = Program("backstop")
        admin, admin_path = localnet.keypair("admin")
        issuer, issuer_path = localnet.keypair("issuer")
        self.k = Keys(
            admin=admin, admin_path=admin_path, feed=localnet.keypair("feed")[0], issuer=issuer,
            issuer_path=issuer_path, co_signer=localnet.keypair("co_signer")[0], alice=localnet.keypair("alice")[0],
            bob=localnet.keypair("bob")[0], backer=localnet.keypair("backer")[0], crank=localnet.keypair("crank")[0],
        )
        self.oracle = load_oracle_key()
        self.price = 0

    # PDAs
    def v(self, *seeds):
        return pda(self.vault.program_id, *seeds)

    def b(self, *seeds):
        return pda(self.backstop.program_id, *seeds)

    @property
    def market(self) -> Pubkey:
        return self.v(b"market", self.coll_mint)

    def send(self, ixs, signers, label):
        return self.rpc.send(ixs, signers, label)

    def market_state(self) -> dict:
        return self.vault.decode_account("Market", self.rpc.account(self.market))

    def claim_state(self, record: Pubkey) -> dict:
        return self.backstop.decode_account("Claim", self.rpc.account(self.b(b"claim", record)))

    # --- setup ---------------------------------------------------------------------------------------------------

    def bootstrap(self, start_price_usd: float, params: dict = MARKET_PARAMS) -> None:
        k = self.k
        for who in (k.admin, k.feed, k.issuer, k.co_signer, k.alice, k.bob, k.backer, k.crank):
            self.rpc.airdrop(who.pubkey(), 20 * 10**9)
        say("funded demo wallets with localnet SOL")

        self.coll_mint = tokens.create_aaplx_mint(localnet.keypair("aaplx_mint")[1], k.issuer_path, LIVE_MULTIPLIER, self.rpc.url)
        self.usdc_mint = tokens.create_usdc_mint(localnet.keypair("usdc_mint")[1], k.issuer_path, self.rpc.url)
        say(f"mock AAPLx (Token-2022, scaled UI amount {LIVE_MULTIPLIER}, pausable) {self.coll_mint}")
        say(f"mock USDC {self.usdc_mint}")

        vcfg = self.v(b"vconfig")
        self.send([self.vault.instruction(
            "initialize_config",
            {"admin": k.admin.pubkey(), "program": self.vault.program_id,
             "program_data": pda(LOADER_V3, self.vault.program_id), "config": vcfg, "system_program": SYSTEM},
            {"feed_authority": k.feed.pubkey(), "cluster_tag": CLUSTER_DEVNET},
        )], [k.admin], "vault initialize_config")
        market = self.market
        self.send([self.vault.instruction(
            "create_market",
            {"admin": k.admin.pubkey(), "config": vcfg, "collateral_mint": self.coll_mint, "usdc_mint": self.usdc_mint,
             "market": market, "collateral_vault": self.v(b"coll_vault", market), "usdc_vault": self.v(b"usdc_vault", market),
             "collateral_token_program": tokens.TOKEN_2022, "usdc_token_program": tokens.TOKEN_CLASSIC, "system_program": SYSTEM},
            {"params": params},
        )], [k.admin], "create_market")
        say(f"vault initialized (cluster tag {CLUSTER_DEVNET} = devnet) and AAPLx market created")

        bcfg = self.b(b"bconfig")
        self.send([self.backstop.instruction(
            "initialize_backstop",
            {"admin": k.admin.pubkey(), "program": self.backstop.program_id,
             "program_data": pda(LOADER_V3, self.backstop.program_id), "config": bcfg, "usdc_mint": self.usdc_mint,
             "usdc_vault": self.b(b"busdc"), "usdc_token_program": tokens.TOKEN_CLASSIC, "system_program": SYSTEM},
            {"verdict_oracle": self.oracle.pubkey(), "co_signer": k.co_signer.pubkey(), "cluster_tag": CLUSTER_DEVNET,
             "per_claim_cap_bps": 1_000, "withdraw_delay_secs": 0},
        )], [k.admin], "initialize_backstop")
        self.send([self.backstop.instruction("set_claim_timing", {"admin": k.admin.pubkey(), "config": bcfg}, DEMO_TIMING)],
                  [k.admin], "set_claim_timing")
        self.send([self.vault.instruction("set_pool_liquidator", {"admin": k.admin.pubkey(), "config": vcfg},
                                          {"pool_liquidator": bcfg})], [k.admin], "set_pool_liquidator")
        say(f"backstop initialized; demo claim timings {DEMO_TIMING}; backstop registered as the pool liquidator")

        backer_usdc = tokens.fund(self.usdc_mint, k.backer.pubkey(), 100_000 * USDC, k.issuer_path, tokens.TOKEN_CLASSIC, self.rpc.url)
        backer_pda = self.b(b"backer", k.backer.pubkey())
        self.send([
            self.backstop.instruction("open_backer", {"owner": k.backer.pubkey(), "config": bcfg, "backer": backer_pda, "system_program": SYSTEM}),
            self.backstop.instruction("deposit", {"owner": k.backer.pubkey(), "config": bcfg, "backer": backer_pda, "usdc_mint": self.usdc_mint,
                                                  "owner_usdc": backer_usdc, "usdc_vault": self.b(b"busdc"),
                                                  "usdc_token_program": tokens.TOKEN_CLASSIC}, {"amount": 100_000 * USDC}),
        ], [k.backer], "backer deposit")
        self.send([self.backstop.instruction(
            "open_inventory",
            {"payer": k.admin.pubkey(), "market": market, "inventory": self.b(b"inv", market), "collateral_mint": self.coll_mint,
             "config": bcfg, "inventory_vault": self.b(b"invault", market), "collateral_token_program": tokens.TOKEN_2022,
             "system_program": SYSTEM},
        )], [k.admin], "open_inventory")
        say("backers deposited 100,000 USDC into the backstop")

        bob_usdc = tokens.fund(self.usdc_mint, k.bob.pubkey(), 50_000 * USDC, k.issuer_path, tokens.TOKEN_CLASSIC, self.rpc.url)
        supplier = self.v(b"supplier", market, k.bob.pubkey())
        self.send([
            self.vault.instruction("open_supplier", {"owner": k.bob.pubkey(), "market": market, "supplier": supplier, "system_program": SYSTEM}),
            self.vault.instruction("supply", {"owner": k.bob.pubkey(), "config": vcfg, "market": market, "supplier": supplier,
                                              "usdc_mint": self.usdc_mint, "owner_usdc": bob_usdc,
                                              "usdc_vault": self.v(b"usdc_vault", market), "usdc_token_program": tokens.TOKEN_CLASSIC},
                                   {"amount": 50_000 * USDC}),
        ], [k.bob], "supply")
        say("a lender supplied 50,000 USDC to the market")

        self.push_price(int(round(start_price_usd * PRICE_SCALE)))
        # spl-token mints UI units, which Scaled UI Amount divides by the multiplier; mint 11 UI so at least 10 raw
        # shares exist, then deposit exactly 10 raw.
        self.alice_coll = tokens.fund(self.coll_mint, k.alice.pubkey(), 11 * ONE_SHARE, k.issuer_path, tokens.TOKEN_2022, self.rpc.url)
        self.alice_usdc = tokens.fund(self.usdc_mint, k.alice.pubkey(), 0, k.issuer_path, tokens.TOKEN_CLASSIC, self.rpc.url)
        position = self.v(b"position", market, k.alice.pubkey())
        borrow_usdc = self.max_borrow_usdc(10 * ONE_SHARE, self.price) * 98 // 100
        self.send([
            self.vault.instruction("open_position", {"owner": k.alice.pubkey(), "market": market, "position": position, "system_program": SYSTEM}),
            self.vault.instruction("deposit_collateral", {"owner": k.alice.pubkey(), "market": market, "position": position,
                                                          "collateral_mint": self.coll_mint, "owner_collateral": self.alice_coll,
                                                          "collateral_vault": self.v(b"coll_vault", market),
                                                          "collateral_token_program": tokens.TOKEN_2022}, {"amount": 10 * ONE_SHARE}),
            self.vault.instruction("borrow", {"owner": k.alice.pubkey(), "config": vcfg, "market": market, "position": position,
                                              "collateral_mint": self.coll_mint, "usdc_mint": self.usdc_mint, "owner_usdc": self.alice_usdc,
                                              "usdc_vault": self.v(b"usdc_vault", market), "usdc_token_program": tokens.TOKEN_CLASSIC},
                                   {"amount": borrow_usdc}),
        ], [k.alice], "alice opens a loan")
        say(f"borrower deposited 10 AAPLx at {fmt_usd(self.price)} and borrowed {borrow_usdc / USDC:,.2f} USDC (98% of the 40% limit)")

    def max_borrow_usdc(self, raw: int, price_fp8: int) -> int:
        value_usd6 = raw * LIVE_MULTIPLIER * price_fp8 / PRICE_SCALE / ONE_SHARE * USDC
        return int(value_usd6 * MARKET_PARAMS["ltv_bps"] / 10_000)

    # --- actions -------------------------------------------------------------------------------------------------

    def push_price(self, price_fp8: int, market_open: bool = True) -> None:
        ix = self.vault.instruction("push_price", {"feed_authority": self.k.feed.pubkey(), "config": self.v(b"vconfig"), "market": self.market,
                                                   "collateral_mint": self.coll_mint},
                                    {"price": price_fp8, "market_open": market_open, "last_close": price_fp8})
        for _ in range(10):
            try:
                self.send([ix], [self.k.feed], "push_price")
                self.price = price_fp8
                return
            except RpcError as e:
                if "StalePriceUpdate" not in str(e):
                    raise
                time.sleep(1.0)  # the vault accepts one update per validator second
        raise RuntimeError("push_price kept failing")

    def walk_price_to(self, target_fp8: int, on_step=None) -> int:
        steps = 0
        while self.price > target_fp8:
            nxt = max(self.price * 996 // 1_000, target_fp8)
            time.sleep(1.05)
            self.push_price(nxt)
            steps += 1
            if self.market_state()["price"]["flagged"]:
                raise RuntimeError("the walk tripped the spike guard; steps must stay inside the cap")
            if on_step:
                on_step(nxt)
        return steps

    def pool_liquidate(self) -> tuple[Pubkey, dict]:
        k, market = self.k, self.market
        crank_usdc = tokens.fund(self.usdc_mint, k.crank.pubkey(), 0, k.issuer_path, tokens.TOKEN_CLASSIC, self.rpc.url)
        seq = self.market_state()["liq_seq"]
        record = self.v(b"liq", market, seq)
        self.send([self.backstop.instruction(
            "pool_liquidate",
            {"payer": k.crank.pubkey(), "config": self.b(b"bconfig"), "vault_config": self.v(b"vconfig"), "market": market,
             "position": self.v(b"position", market, k.alice.pubkey()), "record": record, "collateral_mint": self.coll_mint,
             "usdc_mint": self.usdc_mint, "pool_usdc_vault": self.b(b"busdc"), "inventory": self.b(b"inv", market),
             "inventory_vault": self.b(b"invault", market), "vault_collateral_vault": self.v(b"coll_vault", market),
             "vault_usdc_vault": self.v(b"usdc_vault", market), "crank_usdc": crank_usdc,
             "collateral_token_program": tokens.TOKEN_2022, "usdc_token_program": tokens.TOKEN_CLASSIC,
             "system_program": SYSTEM, "stock_vault_program": self.vault.program_id},
            {"repay_amount": 50_000 * USDC},
        )], [k.crank], "pool_liquidate")
        return record, self.vault.decode_account("LiquidationRecord", self.rpc.account(record))

    def submit_verdict(self, record: Pubkey, rec: dict, recorder: Recorder) -> dict:
        k, market = self.k, self.market
        borrower = k.alice.pubkey()
        bclaims = self.b(b"bclaims", market, borrower)
        if self.rpc.account(bclaims) is None:
            self.send([self.backstop.instruction("open_borrower_claims", {"payer": k.admin.pubkey(), "market": market,
                                                                          "borrower": borrower, "borrower_claims": bclaims,
                                                                          "system_program": SYSTEM})],
                      [k.admin], "open_borrower_claims")
        signed = sign_verdict(
            liquidation_record=record, borrower=borrower, liquidated_at=rec["ts"], symbol="AAPLX", oracle_key=self.oracle,
            recordings_dir=recorder.dir, min_after_wait_secs=DEMO_TIMING["min_after_wait_secs"],
        )
        say(f"verdict engine signed facts: reference at liquidation {fmt_usd(signed.args.ref_at_liq)}, "
            f"reference after the wait {fmt_usd(signed.args.ref_after)}")
        ed_ix, facts_ix = build_submit_facts_instructions(
            signed, SubmitFactsAccounts(payer=k.admin.pubkey(), market=market, liquidation_record=record, borrower=borrower, payout=rec["payout"]),
        )
        self.send([ed_ix, facts_ix], [k.admin], "submit_facts")
        return self.claim_state(record)

    def wait_until_chain_time(self, ts: int) -> None:
        while self.rpc.unix_time() < ts:
            time.sleep(1)


# --- scenarios -------------------------------------------------------------------------------------------------


def wait_for_reference_wait(demo: Demo, recorder: Recorder, rec: dict, truth: float, not_before_wall: float = 0) -> None:
    """Samples the after-reference once the minimum wait has passed on both the chain clock (the program checks
    after_ts against the record's chain timestamp) and the wall clock (the engine and recorder use it)."""
    ready = rec["ts"] + DEMO_TIMING["min_after_wait_secs"] + 5
    say(f"waiting at least {DEMO_TIMING['min_after_wait_secs']}s so the after-reference can prove whether the move held")
    demo.wait_until_chain_time(ready)
    while time.time() < max(ready, not_before_wall):
        time.sleep(1)
    recorder.sample(truth if truth != recorder.real_price else None)


def scenario_wrongful(demo: Demo, recorder: Recorder) -> None:
    truth = recorder.real_price
    demo.bootstrap(truth)
    recorder.sample()
    say(f"real reference price (replayed CMC recording): ${truth:,.2f}")
    target = int(round(truth * PRICE_SCALE)) * 70 // 100
    say(f"BAD DATA: the feed drifts down 0.4% per update toward {fmt_usd(target)} while the real price stays put")
    last_sample = time.monotonic()

    def keep_recording(_):
        nonlocal last_sample
        if time.monotonic() - last_sample > 20:
            recorder.sample()
            last_sample = time.monotonic()

    steps = demo.walk_price_to(target, keep_recording)
    say(f"feed walked {steps} updates to {fmt_usd(demo.price)} without tripping the 5% spike guard")
    recorder.sample()
    record, rec = demo.pool_liquidate()
    say(f"POOL LIQUIDATED the borrower: seized {rec['seized_raw'] / ONE_SHARE:.4f} AAPLx for "
        f"{rec['debt_repaid'] / USDC:,.2f} USDC at TWAP {fmt_usd(rec['price_fp'])}")
    wait_for_reference_wait(demo, recorder, rec, truth)

    before = demo.rpc.token_balance(demo.alice_usdc)
    claim = demo.submit_verdict(record, rec, recorder)
    say(f"claim recorded on-chain: status {claim['status']}, exact loss {claim['loss'] / USDC:,.6f} USDC")
    if claim["status"] == "Denied":
        raise SystemExit(f"FAIL: wrongful liquidation was denied (reason {claim['deny_reason']})")
    if claim["status"] == "PendingTime":
        say(f"loan was under the demo gate ({DEMO_TIMING['gate_secs']}s): claim held at full value until the gate clears")
        demo.wait_until_chain_time(claim["releasable_at"])
        demo.send([demo.backstop.instruction("unlock_claim", {"config": demo.b(b"bconfig"), "claim": demo.b(b"claim", record)})],
                  [demo.k.crank], "unlock_claim")
        claim = demo.claim_state(record)
        say(f"gate cleared: claim {claim['status']}")
    say(f"cooldown {DEMO_TIMING['cooldown_secs']}s, then a {DEMO_TIMING['stream_secs']}s linear payout stream")
    demo.wait_until_chain_time(claim["stream_end"] + 1)
    stream_ix = demo.backstop.instruction(
        "claim_stream",
        {"config": demo.b(b"bconfig"), "claim": demo.b(b"claim", record), "usdc_mint": demo.usdc_mint,
         "payout_usdc": demo.alice_usdc, "usdc_vault": demo.b(b"busdc"), "usdc_token_program": tokens.TOKEN_CLASSIC},
    )
    demo.send([stream_ix], [demo.k.crank], "claim_stream")
    claim = demo.claim_state(record)
    paid = demo.rpc.token_balance(demo.alice_usdc) - before
    say(f"PAID BACK: borrower received {paid / USDC:,.6f} USDC; claim {claim['status']}")
    if claim["status"] != "Completed" or paid != claim["loss"]:
        raise SystemExit(f"FAIL: expected Completed and paid == loss ({claim['loss']}), got {claim['status']} / {paid}")
    say("PASS: wrongful liquidation paid back 1:1, exactly the on-chain loss")


def scenario_real_crash(demo: Demo, recorder: Recorder) -> None:
    start = recorder.real_price
    demo.bootstrap(start)
    recorder.sample()
    pre_sample_wall = time.time()
    say(f"reference price before the fall (replayed CMC recording): ${start:,.2f}")
    target = int(round(start * PRICE_SCALE)) * 70 // 100
    say(f"REAL CRASH: the market genuinely falls toward {fmt_usd(target)}; the feed reports it faithfully")
    steps = demo.walk_price_to(target)
    say(f"feed followed the fall over {steps} updates to {fmt_usd(demo.price)}")
    record, rec = demo.pool_liquidate()
    say(f"POOL LIQUIDATED the borrower at TWAP {fmt_usd(rec['price_fp'])}")
    say("the reference recorder had not sampled since before the fall (cron cadence), so the reference at the "
        "moment of liquidation still shows the old price -- check (a) alone would call this wrong")
    crashed = demo.price / PRICE_SCALE
    # The next recorder sample lands later than the last one was before the liquidation, so the pre-fall sample stays
    # the closest one to the liquidation (the reference lag being demonstrated), with a 15 s clock-skew margin.
    liq_wall = time.time()
    next_sample_wall = liq_wall + (liq_wall - pre_sample_wall) + 15
    wait_for_reference_wait(demo, recorder, rec, crashed, not_before_wall=next_sample_wall)
    claim = demo.submit_verdict(record, rec, recorder)
    reason = {1: "PRICE_NOT_WRONG", 2: "MOVE_HELD", 3: "ZERO_LOSS", 4: "PENALTY_ACTIVE"}.get(claim["deny_reason"], claim["deny_reason"])
    say(f"claim recorded on-chain: status {claim['status']}, reason {reason}, loss {claim['loss']}")
    if claim["status"] != "Denied" or reason != "MOVE_HELD":
        raise SystemExit(f"FAIL: expected Denied / MOVE_HELD, got {claim['status']} / {reason}")
    say("PASS: the after-reference confirmed the fall was real -- denied on-chain with MOVE_HELD, nothing paid")


def expect_refused(action, error_name: str, what: str) -> None:
    try:
        action()
    except RpcError as e:
        if error_name in str(e):
            say(f"refused as intended: {what} -> {error_name}")
            return
        raise SystemExit(f"FAIL: {what} failed, but not with {error_name}: {e}")
    raise SystemExit(f"FAIL: {what} succeeded; it must be refused with {error_name}")


def scenario_split(demo: Demo, recorder: Recorder) -> None:
    # Disclosed demo params: the issuer-recommended ±15 min pause shortened to ±30 s so the window fits a live run.
    params = dict(MARKET_PARAMS, activation_pause_secs=30, split_max_hold_secs=600)
    start = recorder.real_price
    demo.bootstrap(start, params)
    k = demo.k
    lead = 75
    activation = demo.rpc.unix_time() + lead
    post_split = LIVE_MULTIPLIER * 4
    tokens.schedule_multiplier(demo.coll_mint, post_split, activation, k.issuer_path, demo.rpc.url)
    say(f"issuer schedules a 4:1 split: multiplier {LIVE_MULTIPLIER} -> {post_split:.7f} at chain time {activation} "
        f"(in {lead}s); the vault holds liquidations for ±{params['activation_pause_secs']}s around it")

    # The classic desync: the feed publishes the post-split share price before the multiplier activates.
    demo.wait_until_chain_time(activation - 20)
    quarter = demo.price // 4
    demo.push_price(quarter)
    time.sleep(1.05)
    demo.push_price(quarter)
    st = demo.market_state()["price"]
    if not st["flagged"] or st["last_price"] == quarter:
        raise SystemExit(f"FAIL: the early post-split price entered the price history (flagged={st['flagged']})")
    say(f"DESYNC: feed reports the post-split price {fmt_usd(quarter)} early, twice; it is exactly the split ratio "
        "away, so it stays flagged and never enters the price history")

    def liquidate():
        demo.pool_liquidate()

    expect_refused(liquidate, "CorporateActionHold", "pool liquidation inside the split window")
    demo.wait_until_chain_time(activation + params["activation_pause_secs"] + 2)
    say("split activated and the ±30 s pause has passed; no market-open price since activation yet")
    expect_refused(liquidate, "CorporateActionHold", "pool liquidation after activation, before a fresh market-open price")

    # The vault re-quotes its stored price history in the new multiplier at activation (split-adjusted, as equity
    # data vendors do), so one ordinary market-open print at the post-split price is accepted as-is.
    demo.push_price(quarter, market_open=True)
    st = demo.market_state()
    if st["price"]["flagged"] or st["price"]["last_update"] <= activation:
        raise SystemExit(f"FAIL: repricing not accepted (flagged={st['price']['flagged']}, last_update={st['price']['last_update']})")
    say(f"feed publishes one market-open price after activation: {fmt_usd(quarter)}, accepted first time because the "
        "stored history was re-quoted for the split")
    expect_refused(liquidate, "NotLiquidatable", "pool liquidation once repriced (the loan is healthy)")
    if demo.market_state()["liq_seq"] != 0:
        raise SystemExit("FAIL: a liquidation record exists")
    say("PASS: the split never liquidated anyone -- held through the window, healthy after it")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("scenario", choices=["wrongful", "real-crash", "split"])
    parser.add_argument("--keep", action="store_true", help="leave the validator running afterwards")
    parser.add_argument("--recordings", type=Path, default=REPO / "verdict" / "recordings" / "aaplx.jsonl")
    a = parser.parse_args()

    admin_path = localnet.keypair("admin")[1]
    proc = localnet.start(admin_path)
    say("local validator up with both programs (upgrade authority = demo admin)")
    try:
        demo = Demo(Rpc())
        recorder = Recorder(localnet.DEMO_DIR / f"recordings-{a.scenario}", a.recordings)
        {"wrongful": scenario_wrongful, "real-crash": scenario_real_crash, "split": scenario_split}[a.scenario](demo, recorder)
    finally:
        if not a.keep:
            proc.terminate()
            proc.wait(timeout=30)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
