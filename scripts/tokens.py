"""Mock tokens for the demo, created through the official `spl-token` CLI (5.5.0, verified locally 2026-09-13).

The mock AAPLx mint carries the extensions the live xStocks mint has that the vault reads: Token-2022 Scaled UI
Amount (the corporate-action multiplier) and Pausable. Decimals match the live mint (8). USDC is a classic SPL
token with 6 decimals. Devnet has no xStocks, so this is the disclosed stand-in (spec 10).
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

from rpc import LOCALNET
from solders.pubkey import Pubkey

AAPLX_DECIMALS = 8
USDC_DECIMALS = 6
TOKEN_2022 = Pubkey.from_string("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")
TOKEN_CLASSIC = Pubkey.from_string("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
# Program IDs copied from the installed spl-*-interface crate sources, never hand-typed.
ATA_PROGRAM = Pubkey.from_string("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")


def _spl(args: list[str], url: str) -> str:
    out = subprocess.run(["spl-token", "-u", url, *args], capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(f"spl-token {' '.join(args)} failed:\n{out.stdout}\n{out.stderr}")
    return out.stdout


def ata(owner: Pubkey, mint: Pubkey, token_program: Pubkey) -> Pubkey:
    return Pubkey.find_program_address([bytes(owner), bytes(token_program), bytes(mint)], ATA_PROGRAM)[0]


def create_aaplx_mint(mint_keypair: Path, issuer_keypair: Path, multiplier: float, url: str = LOCALNET) -> Pubkey:
    out = _spl(
        [
            "--program-2022", "create-token", str(mint_keypair),
            "--decimals", str(AAPLX_DECIMALS),
            "--enable-pause",
            "--ui-amount-multiplier", repr(multiplier),
            "--fee-payer", str(issuer_keypair),
            "--mint-authority", _pubkey_of(issuer_keypair),
        ],
        url,
    )
    return _address_from(out)


def create_usdc_mint(mint_keypair: Path, issuer_keypair: Path, url: str = LOCALNET) -> Pubkey:
    out = _spl(
        [
            "create-token", str(mint_keypair),
            "--decimals", str(USDC_DECIMALS),
            "--fee-payer", str(issuer_keypair),
            "--mint-authority", _pubkey_of(issuer_keypair),
        ],
        url,
    )
    return _address_from(out)


def fund(mint: Pubkey, owner: Pubkey, raw_amount: int, issuer_keypair: Path, token_program: Pubkey, url: str = LOCALNET) -> Pubkey:
    """Creates the owner's associated token account (if needed) and mints `raw_amount` base units into it."""
    flag = ["--program-2022"] if token_program == TOKEN_2022 else []
    account = ata(owner, mint, token_program)
    try:
        _spl([*flag, "create-account", str(mint), "--owner", str(owner), "--fee-payer", str(issuer_keypair)], url)
    except RuntimeError as e:
        if "already in use" not in str(e) and "already exists" not in str(e):
            raise
    if raw_amount > 0:
        decimals = AAPLX_DECIMALS if token_program == TOKEN_2022 else USDC_DECIMALS
        ui = f"{raw_amount / 10**decimals:.{decimals}f}"
        _spl(
            [*flag, "mint", str(mint), ui, str(account), "--mint-authority", str(issuer_keypair), "--fee-payer", str(issuer_keypair)],
            url,
        )
    return account


def schedule_multiplier(mint: Pubkey, multiplier: float, effective_ts: int, issuer_keypair: Path, url: str = LOCALNET) -> None:
    """A split / reverse split / dividend step, announced ahead of time as the issuer does."""
    _spl(
        [
            "--program-2022", "update-ui-amount-multiplier",
            "--fee-payer", str(issuer_keypair),
            "--ui-multiplier-authority", str(issuer_keypair),
            str(mint), repr(multiplier), "--", str(effective_ts),
        ],
        url,
    )


def _pubkey_of(keypair_path: Path) -> str:
    out = subprocess.run(["solana-keygen", "pubkey", str(keypair_path)], capture_output=True, text=True, check=True)
    return out.stdout.strip()


def _address_from(cli_output: str) -> Pubkey:
    m = re.search(r"(?:Creating token|Address:)\s+([1-9A-HJ-NP-Za-km-z]{32,44})", cli_output)
    if not m:
        raise RuntimeError(f"could not find the token address in:\n{cli_output}")
    return Pubkey.from_string(m.group(1))
