"""Stocklana P4 devnet demo runner: the same 3 scenarios as demo.py (wrongful / real-crash / split),
run against the ALREADY-DEPLOYED devnet programs and the ALREADY-INITIALIZED VaultConfig /
BackstopConfig, instead of a spun-up local validator.

    SSL_CERT_FILE=$(python3 -c "import certifi; print(certifi.where())") \
        python3 scripts/demo_devnet.py wrongful
    ... real-crash
    ... split

The scenario logic itself (scenario_wrongful / scenario_real_crash / scenario_split, and every
correctness assertion inside them) is imported UNCHANGED from demo.py -- it was proven end-to-end
on a local validator (2026-09-14, re-confirmed 2026-09-15) and must not be re-derived here. Only
`Demo.bootstrap()` is adapted, because devnet already has a live VaultConfig/BackstopConfig
singleton (created 2026-09-15) that a fresh scenario run must reuse, not recreate.

Design (verified against on-chain state before writing this):
  - devnet's VaultConfig.feed_authority already equals its admin (the deployer key) -- unlike the
    local demo's separate throwaway `feed`/`issuer` keypairs, devnet has no separate identity to
    preserve, so the deployer key plays admin + feed + issuer here.
  - Each run creates a FRESH mock AAPLx mint + market (own collateral_mint => own Market PDA), so
    every scenario starts at liq_seq=0 with a clean price/multiplier history -- exactly like
    demo.py's fresh-per-run local validator -- without touching the shared frontend demo market
    (`app/src/devnet-market.json`) that real wallets have already interacted with.
  - The existing devnet mock USDC mint is reused as-is (mint authority = deployer, already
    confirmed on deploy).
  - `set_pool_liquidator` and `set_claim_timing` are one-time BackstopConfig/VaultConfig admin
    calls, applied once per process if the on-chain value doesn't already match (checked by
    reading the account first, never blindly re-sent). They are DEVNET-wide (there is only one
    VaultConfig/BackstopConfig on this deployment), which is fine: nothing mainnet-sensitive is at
    stake, and no claim exists yet on devnet for a shortened gate to affect.
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "scripts"))
sys.path.insert(0, str(REPO / "verdict"))

import demo  # noqa: E402
import tokens  # noqa: E402
from demo import DEMO_TIMING, MARKET_PARAMS, Recorder  # noqa: E402
from idl_client import Program  # noqa: E402
from rpc import Rpc  # noqa: E402
from solders.keypair import Keypair  # noqa: E402
from solders.pubkey import Pubkey  # noqa: E402

DEVNET = "https://api.devnet.solana.com"
KEYS_DIR = Path.home() / "SAFU" / "safucredit-keys"
DEPLOYER_PATH = KEYS_DIR / "devnet-deployer-keypair.json"
MOCK_USDC_MINT = Pubkey.from_string("Cujm3S48mCeQHnNfAEarYfuwoSwgp9gLPFVVrdDVV8wn")
DEMO_KEYS_DIR = REPO / ".demo" / "keys"
SCENARIO_MINTS_DIR = KEYS_DIR / "demo-scenarios"


def load_keypair(path: Path) -> Keypair:
    return Keypair.from_bytes(bytes(json.loads(path.read_text())))


def persistent_actor(name: str) -> Keypair:
    """Devnet-only actor identity, persisted across runs (so SOL/token funding isn't re-spent on
    every retry). Separate filename namespace from the local-validator `.demo/keys/` throwaways."""
    path = DEMO_KEYS_DIR / f"devnet_{name}.json"
    if path.exists():
        return load_keypair(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    kp = Keypair()
    path.write_text(json.dumps(list(bytes(kp))))
    path.chmod(0o600)
    return kp


def fresh_mint_keypair_path(label: str) -> Path:
    """A brand-new keypair file per invocation -- never reused -- so retries can't collide with an
    'already in use' mint account from a previous attempt at the same scenario."""
    SCENARIO_MINTS_DIR.mkdir(parents=True, exist_ok=True)
    path = SCENARIO_MINTS_DIR / f"{label}-{int(time.time())}-{Keypair().pubkey().__str__()[:6]}.json"
    kp = Keypair()
    path.write_text(json.dumps(list(bytes(kp))))
    path.chmod(0o600)
    return path


def fund_sol(rpc: Rpc, payer_path: Path, recipient: Pubkey, min_lamports: int, top_up_lamports: int) -> None:
    bal = rpc.call("getBalance", [str(recipient), {"commitment": "confirmed"}])["value"]
    if bal >= min_lamports:
        return
    out = subprocess.run(
        ["solana", "transfer", str(recipient), str(top_up_lamports / 1_000_000_000),
         "--keypair", str(payer_path), "--url", rpc.url, "--allow-unfunded-recipient",
         "--fee-payer", str(payer_path), "--commitment", "confirmed"],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        raise RuntimeError(f"solana transfer to {recipient} failed:\n{out.stdout}\n{out.stderr}")


class DevnetDemo(demo.Demo):
    def __init__(self, rpc: Rpc):
        self.rpc = rpc
        self.vault = Program("stock_vault")
        self.backstop = Program("backstop")
        deployer = load_keypair(DEPLOYER_PATH)
        self.k = demo.Keys(
            admin=deployer, admin_path=DEPLOYER_PATH,
            feed=deployer,  # devnet VaultConfig.feed_authority already == admin
            issuer=deployer, issuer_path=DEPLOYER_PATH,  # no separate issuer identity on devnet
            co_signer=persistent_actor("co_signer"),
            alice=persistent_actor("alice"), bob=persistent_actor("bob"),
            backer=persistent_actor("backer"), crank=persistent_actor("crank"),
        )
        from engine import load_oracle_key  # local import: verdict/ added to sys.path above
        self.oracle = load_oracle_key()
        self.price = 0

    # --- one-time devnet setup, idempotent -------------------------------------------------------

    def _ensure_pool_liquidator(self) -> None:
        vcfg = self.v(b"vconfig")
        bcfg = self.b(b"bconfig")
        cur = self.vault.decode_account("VaultConfig", self.rpc.account(vcfg))
        if cur["pool_liquidator"] == bcfg:
            demo.say("pool_liquidator already set to the backstop config -- skipping")
            return
        self.send([self.vault.instruction("set_pool_liquidator", {"admin": self.k.admin.pubkey(), "config": vcfg},
                                          {"pool_liquidator": bcfg})], [self.k.admin], "set_pool_liquidator")
        demo.say(f"set_pool_liquidator -> {bcfg} (one-time devnet setup)")

    def _ensure_claim_timing(self) -> None:
        bcfg = self.b(b"bconfig")
        cur = self.backstop.decode_account("BackstopConfig", self.rpc.account(bcfg))
        want = DEMO_TIMING
        if all(cur[k] == v for k, v in want.items()):
            demo.say("claim timing already at demo values -- skipping")
            return
        self.send([self.backstop.instruction("set_claim_timing", {"admin": self.k.admin.pubkey(), "config": bcfg}, want)],
                  [self.k.admin], "set_claim_timing")
        demo.say(f"set_claim_timing -> {want} (one-time devnet setup, applies to every market on this deployment)")

    # --- bootstrap: reuse existing VaultConfig/BackstopConfig, create a fresh market --------------

    def bootstrap(self, start_price_usd: float, params: dict = MARKET_PARAMS) -> None:
        k = self.k
        for who in (k.co_signer, k.alice, k.bob, k.backer, k.crank):
            fund_sol(self.rpc, DEPLOYER_PATH, who.pubkey(), min_lamports=20_000_000, top_up_lamports=40_000_000)
        demo.say("devnet actor wallets funded with SOL (deployer as payer)")

        self._ensure_pool_liquidator()
        self._ensure_claim_timing()

        mint_path = fresh_mint_keypair_path("aaplx")
        self.coll_mint = tokens.create_aaplx_mint(mint_path, DEPLOYER_PATH, demo.LIVE_MULTIPLIER, url=self.rpc.url)
        self.usdc_mint = MOCK_USDC_MINT
        demo.say(f"fresh mock AAPLx (Token-2022, scaled UI amount {demo.LIVE_MULTIPLIER}, pausable) {self.coll_mint}")
        demo.say(f"reusing existing devnet mock USDC {self.usdc_mint}")

        vcfg, bcfg = self.v(b"vconfig"), self.b(b"bconfig")
        market = self.market
        self.send([self.vault.instruction(
            "create_market",
            {"admin": k.admin.pubkey(), "config": vcfg, "collateral_mint": self.coll_mint, "usdc_mint": self.usdc_mint,
             "market": market, "collateral_vault": self.v(b"coll_vault", market), "usdc_vault": self.v(b"usdc_vault", market),
             "collateral_token_program": tokens.TOKEN_2022, "usdc_token_program": tokens.TOKEN_CLASSIC,
             "system_program": demo.SYSTEM},
            {"params": params},
        )], [k.admin], "create_market")
        demo.say(f"AAPLx market created on live devnet: {market}")

        self.send([self.backstop.instruction(
            "open_inventory",
            {"payer": k.admin.pubkey(), "market": market, "inventory": self.b(b"inv", market), "collateral_mint": self.coll_mint,
             "config": bcfg, "inventory_vault": self.b(b"invault", market), "collateral_token_program": tokens.TOKEN_2022,
             "system_program": demo.SYSTEM},
        )], [k.admin], "open_inventory")
        demo.say("inventory opened for this market")

        backer_usdc = tokens.fund(self.usdc_mint, k.backer.pubkey(), 20_000 * demo.USDC, DEPLOYER_PATH, tokens.TOKEN_CLASSIC, self.rpc.url)
        backer_pda = self.b(b"backer", k.backer.pubkey())
        if self.rpc.account(backer_pda) is None:
            self.send([
                self.backstop.instruction("open_backer", {"owner": k.backer.pubkey(), "config": bcfg, "backer": backer_pda, "system_program": demo.SYSTEM}),
                self.backstop.instruction("deposit", {"owner": k.backer.pubkey(), "config": bcfg, "backer": backer_pda, "usdc_mint": self.usdc_mint,
                                                      "owner_usdc": backer_usdc, "usdc_vault": self.b(b"busdc"),
                                                      "usdc_token_program": tokens.TOKEN_CLASSIC}, {"amount": 20_000 * demo.USDC}),
            ], [k.backer], "backer deposit")
            demo.say("devnet backer deposited 20,000 mock USDC into the shared backstop")
        else:
            demo.say("devnet backer already has a Backer account -- skipping open/deposit (reused from a prior run)")

        bob_usdc = tokens.fund(self.usdc_mint, k.bob.pubkey(), 10_000 * demo.USDC, DEPLOYER_PATH, tokens.TOKEN_CLASSIC, self.rpc.url)
        supplier = self.v(b"supplier", market, k.bob.pubkey())
        self.send([
            self.vault.instruction("open_supplier", {"owner": k.bob.pubkey(), "market": market, "supplier": supplier, "system_program": demo.SYSTEM}),
            self.vault.instruction("supply", {"owner": k.bob.pubkey(), "config": vcfg, "market": market, "supplier": supplier,
                                              "usdc_mint": self.usdc_mint, "owner_usdc": bob_usdc,
                                              "usdc_vault": self.v(b"usdc_vault", market),
                                              "usdc_token_program": tokens.TOKEN_CLASSIC}, {"amount": 10_000 * demo.USDC}),
        ], [k.bob], "supply")
        demo.say("devnet lender supplied 10,000 mock USDC to the market")

        self.push_price(int(round(start_price_usd * demo.PRICE_SCALE)))
        self.alice_coll = tokens.fund(self.coll_mint, k.alice.pubkey(), 11 * demo.ONE_SHARE, DEPLOYER_PATH, tokens.TOKEN_2022, self.rpc.url)
        self.alice_usdc = tokens.fund(self.usdc_mint, k.alice.pubkey(), 0, DEPLOYER_PATH, tokens.TOKEN_CLASSIC, self.rpc.url)
        position = self.v(b"position", market, k.alice.pubkey())
        borrow_usdc = self.max_borrow_usdc(10 * demo.ONE_SHARE, self.price) * 98 // 100
        self.send([
            self.vault.instruction("open_position", {"owner": k.alice.pubkey(), "market": market, "position": position, "system_program": demo.SYSTEM}),
            self.vault.instruction("deposit_collateral", {"owner": k.alice.pubkey(), "market": market, "position": position,
                                                          "collateral_mint": self.coll_mint, "owner_collateral": self.alice_coll,
                                                          "collateral_vault": self.v(b"coll_vault", market),
                                                          "collateral_token_program": tokens.TOKEN_2022}, {"amount": 10 * demo.ONE_SHARE}),
            self.vault.instruction("borrow", {"owner": k.alice.pubkey(), "config": vcfg, "market": market, "position": position,
                                              "collateral_mint": self.coll_mint, "usdc_mint": self.usdc_mint, "owner_usdc": self.alice_usdc,
                                              "usdc_vault": self.v(b"usdc_vault", market), "usdc_token_program": tokens.TOKEN_CLASSIC},
                                   {"amount": borrow_usdc}),
        ], [k.alice], "alice opens a loan")
        demo.say(f"devnet borrower deposited 10 AAPLx at {demo.fmt_usd(self.price)} and borrowed {borrow_usdc / demo.USDC:,.2f} USDC (98% of the 40% limit)")


def main() -> int:
    import argparse
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("scenario", choices=["wrongful", "real-crash", "split"])
    parser.add_argument("--recordings", type=Path, default=REPO / "verdict" / "recordings" / "aaplx.jsonl")
    a = parser.parse_args()

    rpc = Rpc(DEVNET)
    if not rpc.healthy():
        raise SystemExit(f"devnet RPC {DEVNET} not healthy")
    demo.say(f"targeting live devnet: stock_vault {Program('stock_vault').program_id}, backstop {Program('backstop').program_id}")

    d = DevnetDemo(rpc)
    recorder = Recorder(REPO / ".demo" / f"recordings-devnet-{a.scenario}", a.recordings)
    {"wrongful": demo.scenario_wrongful, "real-crash": demo.scenario_real_crash, "split": demo.scenario_split}[a.scenario](d, recorder)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
