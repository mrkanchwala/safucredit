import type { Address, Instruction, TransactionSigner } from "@solana/kit";
import { findAssociatedTokenPda as findAtaClassic } from "@solana-program/token";
import {
  findAssociatedTokenPda as findAta2022,
  getCreateAssociatedTokenIdempotentInstructionAsync as getCreateAta2022Idempotent,
} from "@solana-program/token-2022";
import { getCreateAssociatedTokenIdempotentInstructionAsync as getCreateAtaClassicIdempotent } from "@solana-program/token";
import { AAPLX_MINT, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, USDC_MINT } from "./market";

export async function aaplxAta(owner: Address): Promise<Address> {
  const [ata] = await findAta2022({
    owner,
    mint: AAPLX_MINT,
    tokenProgram: TOKEN_2022_PROGRAM,
  });
  return ata;
}

export async function usdcAta(owner: Address): Promise<Address> {
  const [ata] = await findAtaClassic({
    owner,
    mint: USDC_MINT,
    tokenProgram: TOKEN_PROGRAM,
  });
  return ata;
}

/** Idempotent create -- safe to prepend to every tx that touches the ATA, no pre-check needed. */
export async function ensureAaplxAtaIx(
  payer: TransactionSigner,
  owner: Address,
): Promise<Instruction> {
  return getCreateAta2022Idempotent({
    payer,
    owner,
    mint: AAPLX_MINT,
    tokenProgram: TOKEN_2022_PROGRAM,
  });
}

export async function ensureUsdcAtaIx(
  payer: TransactionSigner,
  owner: Address,
): Promise<Instruction> {
  return getCreateAtaClassicIdempotent({
    payer,
    owner,
    mint: USDC_MINT,
    tokenProgram: TOKEN_PROGRAM,
  });
}
