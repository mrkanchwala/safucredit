"""The committed IDL must match the Rust source, and the IDL builder must produce the same bytes as the layout
already proven against the real compiled backstop (verdict/tx.py, verdict/verify_litesvm.py)."""

from __future__ import annotations

import re
import sys
from pathlib import Path

import pytest
from solders.pubkey import Pubkey

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts"))
sys.path.insert(0, str(REPO / "verdict"))

from engine import FactsArgs  # noqa: E402
from idl_client import IdlError, Program  # noqa: E402
from tx import SUBMIT_FACTS_DISCRIMINATOR, SubmitFactsAccounts, _borsh_facts_args, submit_facts_instruction  # noqa: E402


def _rust_instructions(program: str) -> dict[str, list[str]]:
    """instruction name -> ordered account field names, parsed from the program's lib.rs."""
    src = (REPO / "solana" / "programs" / program / "src" / "lib.rs").read_text()
    structs: dict[str, list[str]] = {}
    for m in re.finditer(r"#\[derive\(Accounts\)\]\s*(?:#\[instruction\(.*?\)\]\s*)?pub struct (\w+)<'info> \{(.*?)\n\}", src, re.S):
        structs[m.group(1)] = re.findall(r"^\s*pub (\w+):", m.group(2), re.M)
    out = {}
    for m in re.finditer(r"pub fn (\w+)\(\s*ctx: Context<(\w+)>", src):
        out[m.group(1)] = structs[m.group(2)]
    return out


@pytest.mark.parametrize("program", ["stock_vault", "backstop"])
def test_committed_idl_matches_the_rust_source(program):
    idl = Program(program).idl
    rust = _rust_instructions(program)
    idl_ix = {i["name"]: [a["name"] for a in i["accounts"]] for i in idl["instructions"]}
    assert sorted(idl_ix) == sorted(rust), (
        f"{program} IDL is stale: refresh solana/idl/ (see scripts/idl_client.py docstring)"
    )
    for name, accounts in rust.items():
        assert idl_ix[name] == accounts, f"{program}.{name}: IDL accounts {idl_ix[name]} != Rust {accounts}"


def _facts() -> FactsArgs:
    return FactsArgs(
        liquidation_record=Pubkey.from_bytes(bytes([7]) * 32),
        borrower=Pubkey.from_bytes(bytes([8]) * 32),
        ref_at_liq=33_012_000_000,
        ref_after=33_100_000_000,
        after_ts=1_800_003_600,
        evidence_hash=bytes(range(32)),
        deadline=1_800_000_600,
    )


def test_idl_builder_matches_the_proven_submit_facts_layout():
    backstop = Program("backstop")
    f = _facts()
    accs = SubmitFactsAccounts(
        payer=Pubkey.from_bytes(bytes([1]) * 32),
        market=Pubkey.from_bytes(bytes([2]) * 32),
        liquidation_record=f.liquidation_record,
        borrower=f.borrower,
        payout=f.borrower,
    )
    proven = submit_facts_instruction(f, accs)
    built = backstop.instruction(
        "submit_facts",
        {m_name: m.pubkey for m_name, m in zip(
            [a["name"] for a in backstop.idl["instructions"][[i["name"] for i in backstop.idl["instructions"]].index("submit_facts")]["accounts"]],
            proven.accounts,
        )},
        {"args": {k: getattr(f, k) for k in ("liquidation_record", "borrower", "ref_at_liq", "ref_after", "after_ts", "evidence_hash", "deadline")}},
    )
    assert bytes(built.data) == bytes(proven.data)
    assert bytes(built.data)[:8] == SUBMIT_FACTS_DISCRIMINATOR
    assert bytes(built.data)[8:] == _borsh_facts_args(f)
    assert [(m.pubkey, m.is_signer, m.is_writable) for m in built.accounts] == [
        (m.pubkey, m.is_signer, m.is_writable) for m in proven.accounts
    ]


def test_missing_or_unknown_accounts_and_args_are_refused():
    vault = Program("stock_vault")
    with pytest.raises(IdlError, match="missing accounts"):
        vault.instruction("set_paused", {"admin": Pubkey.default()}, {"paused": True})
    with pytest.raises(IdlError, match="unknown accounts"):
        vault.instruction("set_paused", {"admin": Pubkey.default(), "config": Pubkey.default(), "nope": Pubkey.default()}, {"paused": True})
    with pytest.raises(IdlError, match="expected args"):
        vault.instruction("set_paused", {"admin": Pubkey.default(), "config": Pubkey.default()}, {})
    with pytest.raises(IdlError, match="no instruction"):
        vault.instruction("does_not_exist", {})


def test_struct_args_round_trip():
    vault = Program("stock_vault")
    params = {
        "ltv_bps": 4_000, "liq_threshold_bps": 5_000, "min_liq_bonus_bps": 100, "max_liq_bonus_bps": 500,
        "insolvency_ltv_bps": 9_500, "close_factor_bps": 2_500, "max_liquidation_debt": 100_000_000_000,
        "deviation_cap_bps": 500, "activation_pause_secs": 900, "split_cap_bps": 500, "split_max_hold_secs": 86_400,
        "borrow_max_price_age_secs": 3_600, "liquidation_max_price_age_open_secs": 90_000,
        "max_price_age_closed_secs": 345_600,
        "rate": {"base_bps": 100, "slope1_bps": 500, "slope2_bps": 6_000, "kink_bps": 9_000},
        "borrow_cap": 500_000_000_000, "collateral_cap_raw": 200_000_000_000, "backer_interest_share_bps": 1_500,
    }
    ty = {"defined": {"name": "MarketParams"}}
    raw = vault.encode(ty, params)
    decoded, end = vault.decode(ty, raw)
    assert decoded == params and end == len(raw)
