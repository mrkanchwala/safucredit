"""Local validator for the demo scripts: both programs loaded upgradeable, with the run's admin as upgrade
authority -- the init gate refuses anyone else, exactly as on devnet. Real clock, real transactions, no warp.

Everything a run creates (ledger, keypairs, recordings) lives under `.demo/` (gitignored). Keypairs there are
localnet throwaways; they are never the committed program keys' owners and never leave the machine.
"""

from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path

from rpc import LOCALNET, Rpc
from solders.keypair import Keypair

REPO = Path(__file__).resolve().parents[1]
DEMO_DIR = REPO / ".demo"
DEPLOY = REPO / "solana" / "target" / "deploy"
VAULT_ID = "GkQw6VGDKYBWJtgtWUmFDkHNqGrnyjSviQeQzVMW35K2"
BACKSTOP_ID = "H1hApKnNkYPsqQ9WVZGzqZQZYirxCLpXDj2uEmzMK3YV"


def keypair(name: str) -> tuple[Keypair, Path]:
    """Loads `.demo/keys/<name>.json`, creating it on first use."""
    path = DEMO_DIR / "keys" / f"{name}.json"
    if path.exists():
        return Keypair.from_bytes(bytes(json.loads(path.read_text()))), path
    path.parent.mkdir(parents=True, exist_ok=True)
    kp = Keypair()
    path.write_text(json.dumps(list(bytes(kp))))
    path.chmod(0o600)
    return kp, path


def start(authority_path: Path, url: str = LOCALNET, timeout: float = 90.0) -> subprocess.Popen:
    for so in ("stock_vault.so", "backstop.so"):
        if not (DEPLOY / so).exists():
            raise SystemExit(f"missing {DEPLOY / so}: run cargo build-sbf for both programs first")
    rpc = Rpc(url)
    if rpc.healthy():
        raise SystemExit(f"a validator is already answering at {url}; stop it first (it may hold stale programs)")
    DEMO_DIR.mkdir(exist_ok=True)
    log = open(DEMO_DIR / "validator.log", "w")
    proc = subprocess.Popen(
        [
            "solana-test-validator",
            "--reset",
            "--quiet",
            "--ledger", str(DEMO_DIR / "ledger"),
            "--upgradeable-program", VAULT_ID, str(DEPLOY / "stock_vault.so"), str(authority_path),
            "--upgradeable-program", BACKSTOP_ID, str(DEPLOY / "backstop.so"), str(authority_path),
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise SystemExit(f"validator exited early, see {DEMO_DIR / 'validator.log'}")
        if rpc.healthy():
            return proc
        time.sleep(1)
    proc.terminate()
    raise SystemExit(f"validator not healthy within {timeout}s")
