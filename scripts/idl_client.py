"""Instruction builder and account decoder driven by the programs' Anchor IDL (solana/idl/*.json).

Anchor resolves accounts by position. Hand-typed account lists drift silently when a program grows an
account (it happened to verdict/tx.py in phase 4), so every script builds instructions from the IDL instead:
accounts are passed by name and ordered here, a missing or unknown name raises, and argument bytes come from
the IDL's own type definitions. `scripts/tests/test_idl_client.py` fails if the committed IDL falls behind the
Rust source.

Refresh the IDL after any program change:
    cd solana && anchor idl build -p stock_vault -o idl/stock_vault.json && anchor idl build -p backstop -o idl/backstop.json
"""

from __future__ import annotations

import json
import struct
from pathlib import Path
from typing import Any

from solders.instruction import AccountMeta, Instruction
from solders.pubkey import Pubkey

IDL_DIR = Path(__file__).resolve().parents[1] / "solana" / "idl"

_INT_FORMATS = {
    "u8": ("<B", 1),
    "i8": ("<b", 1),
    "u16": ("<H", 2),
    "i16": ("<h", 2),
    "u32": ("<I", 4),
    "i32": ("<i", 4),
    "u64": ("<Q", 8),
    "i64": ("<q", 8),
}


class IdlError(Exception):
    pass


class Program:
    def __init__(self, name: str, idl_dir: Path = IDL_DIR):
        self.idl = json.loads((idl_dir / f"{name}.json").read_text())
        self.program_id = Pubkey.from_string(self.idl["address"])
        self._ix = {i["name"]: i for i in self.idl["instructions"]}
        self._types = {t["name"]: t["type"] for t in self.idl.get("types", [])}
        self._accounts = {a["name"]: bytes(a["discriminator"]) for a in self.idl.get("accounts", [])}

    # --- instructions ---------------------------------------------------------------------------

    def instruction(self, name: str, accounts: dict[str, Pubkey], args: dict[str, Any] | None = None) -> Instruction:
        spec = self._ix.get(name)
        if spec is None:
            raise IdlError(f"{self.idl['metadata']['name']} has no instruction {name!r}")
        args = args or {}
        wanted = [a["name"] for a in spec["accounts"]]
        unknown = set(accounts) - set(wanted)
        missing = [n for n in wanted if n not in accounts]
        if unknown or missing:
            raise IdlError(f"{name}: unknown accounts {sorted(unknown)}, missing accounts {missing}")
        metas = [
            AccountMeta(accounts[a["name"]], is_signer=bool(a.get("signer")), is_writable=bool(a.get("writable")))
            for a in spec["accounts"]
        ]
        arg_names = [a["name"] for a in spec["args"]]
        if set(args) != set(arg_names):
            raise IdlError(f"{name}: expected args {arg_names}, got {sorted(args)}")
        data = bytes(spec["discriminator"]) + b"".join(self.encode(a["type"], args[a["name"]]) for a in spec["args"])
        return Instruction(program_id=self.program_id, data=data, accounts=metas)

    # --- borsh ---------------------------------------------------------------------------------

    def encode(self, ty: Any, value: Any) -> bytes:
        if isinstance(ty, str):
            if ty in _INT_FORMATS:
                return struct.pack(_INT_FORMATS[ty][0], value)
            if ty in ("u128", "i128"):
                return int(value).to_bytes(16, "little", signed=ty == "i128")
            if ty == "bool":
                return b"\x01" if value else b"\x00"
            if ty == "pubkey":
                return bytes(value)
            raise IdlError(f"unsupported type {ty}")
        if "array" in ty:
            inner, length = ty["array"]
            if inner == "u8":
                raw = bytes(value)
                if len(raw) != length:
                    raise IdlError(f"expected {length} bytes, got {len(raw)}")
                return raw
            if len(value) != length:
                raise IdlError(f"expected array of {length}, got {len(value)}")
            return b"".join(self.encode(inner, v) for v in value)
        if "option" in ty:
            return b"\x00" if value is None else b"\x01" + self.encode(ty["option"], value)
        if "vec" in ty:
            return struct.pack("<I", len(value)) + b"".join(self.encode(ty["vec"], v) for v in value)
        if "defined" in ty:
            spec = self._types[ty["defined"]["name"]]
            if spec["kind"] == "struct":
                return b"".join(self.encode(f["type"], value[f["name"]]) for f in spec["fields"])
            if spec["kind"] == "enum":
                names = [v["name"] for v in spec["variants"]]
                return struct.pack("<B", names.index(value))
        raise IdlError(f"unsupported type {ty}")

    def decode(self, ty: Any, data: bytes, offset: int = 0) -> tuple[Any, int]:
        if isinstance(ty, str):
            if ty in _INT_FORMATS:
                fmt, size = _INT_FORMATS[ty]
                return struct.unpack_from(fmt, data, offset)[0], offset + size
            if ty in ("u128", "i128"):
                return int.from_bytes(data[offset : offset + 16], "little", signed=ty == "i128"), offset + 16
            if ty == "bool":
                return data[offset] != 0, offset + 1
            if ty == "pubkey":
                return Pubkey.from_bytes(data[offset : offset + 32]), offset + 32
            raise IdlError(f"unsupported type {ty}")
        if "array" in ty:
            inner, length = ty["array"]
            if inner == "u8":
                return data[offset : offset + length], offset + length
            out = []
            for _ in range(length):
                v, offset = self.decode(inner, data, offset)
                out.append(v)
            return out, offset
        if "option" in ty:
            if data[offset] == 0:
                return None, offset + 1
            return self.decode(ty["option"], data, offset + 1)
        if "defined" in ty:
            spec = self._types[ty["defined"]["name"]]
            if spec["kind"] == "struct":
                out = {}
                for f in spec["fields"]:
                    out[f["name"]], offset = self.decode(f["type"], data, offset)
                return out, offset
            if spec["kind"] == "enum":
                return spec["variants"][data[offset]]["name"], offset + 1
        raise IdlError(f"unsupported type {ty}")

    def decode_account(self, account_name: str, data: bytes) -> dict:
        disc = self._accounts.get(account_name)
        if disc is None:
            raise IdlError(f"no account type {account_name!r}")
        if data[:8] != disc:
            raise IdlError(f"account data is not a {account_name} (discriminator mismatch)")
        value, _ = self.decode({"defined": {"name": account_name}}, data, 8)
        return value


def pda(program: Pubkey, *seeds: bytes | Pubkey | int) -> Pubkey:
    """Seeds: bytes as-is, pubkeys as their 32 bytes, ints as u64 little-endian (Anchor's `to_le_bytes()`)."""
    parts = []
    for s in seeds:
        if isinstance(s, Pubkey):
            parts.append(bytes(s))
        elif isinstance(s, int):
            parts.append(s.to_bytes(8, "little"))
        else:
            parts.append(s)
    return Pubkey.find_program_address(parts, program)[0]
