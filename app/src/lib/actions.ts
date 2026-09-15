import type { Address, Instruction, TransactionSigner } from "@solana/kit";
import {
  getDepositCollateralInstructionAsync,
  getBorrowInstructionAsync,
  getRepayInstructionAsync,
  getWithdrawCollateralInstructionAsync,
  getSupplyInstructionAsync,
  getWithdrawSupplyInstructionAsync,
  getOpenPositionInstructionAsync,
  getOpenSupplierInstructionAsync,
  getSetPayoutAddressInstructionAsync,
  findPositionPda,
} from "../generated/stock_vault";
import {
  getDepositInstructionAsync as getBackerDepositInstructionAsync,
  getRequestWithdrawInstructionAsync,
  getFinalizeWithdrawInstructionAsync,
  getOpenBackerInstructionAsync,
  getClaimStreamInstructionAsync,
} from "../generated/backstop";
import { AAPLX_MINT, MARKET, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, USDC_MINT } from "./market";
import { aaplxAta, ensureAaplxAtaIx, ensureUsdcAtaIx, usdcAta } from "./ata";
import { readBacker, readPosition, readSupplier } from "./reads";
import type { AppClient } from "./client";

type Signer = TransactionSigner;

async function send(client: AppClient, ixs: Instruction[], abortSignal?: AbortSignal) {
  const result = await client.sendTransaction(ixs, { abortSignal });
  return result.context.signature;
}

/** Mirrors the mockup's single "Deposit & Borrow" button: open the position if this is the
 *  wallet's first time in the market, deposit AAPLx collateral, then borrow USDC -- one signature. */
export async function depositAndBorrow(
  client: AppClient,
  owner: Signer,
  collateralRaw: bigint,
  borrowRaw: bigint,
): Promise<string> {
  const [positionPda] = await findPositionPda({ market: MARKET, owner: owner.address });
  const ownerCollateral = await aaplxAta(owner.address);
  const ownerUsdc = await usdcAta(owner.address);

  const ixs: Instruction[] = [
    await ensureAaplxAtaIx(owner, owner.address),
    await ensureUsdcAtaIx(owner, owner.address),
  ];
  const existingPosition = await readPosition(client.rpc, owner.address);
  if (!existingPosition) {
    ixs.push(await getOpenPositionInstructionAsync({ owner, market: MARKET, position: positionPda }));
  }

  if (collateralRaw > 0n) {
    ixs.push(
      await getDepositCollateralInstructionAsync({
        owner,
        market: MARKET,
        position: positionPda,
        collateralMint: AAPLX_MINT,
        ownerCollateral,
        collateralTokenProgram: TOKEN_2022_PROGRAM,
        amount: collateralRaw,
      }),
    );
  }
  if (borrowRaw > 0n) {
    ixs.push(
      await getBorrowInstructionAsync({
        owner,
        market: MARKET,
        position: positionPda,
        collateralMint: AAPLX_MINT,
        usdcMint: USDC_MINT,
        ownerUsdc,
        usdcTokenProgram: TOKEN_PROGRAM,
        amount: borrowRaw,
      }),
    );
  }
  return send(client, ixs);
}

export async function repay(client: AppClient, owner: Signer, amountRaw: bigint): Promise<string> {
  const [positionPda] = await findPositionPda({ market: MARKET, owner: owner.address });
  const payerUsdc = await usdcAta(owner.address);
  const ix = await getRepayInstructionAsync({
    payer: owner,
    market: MARKET,
    position: positionPda,
    usdcMint: USDC_MINT,
    payerUsdc,
    usdcTokenProgram: TOKEN_PROGRAM,
    amount: amountRaw,
  });
  return send(client, [await ensureUsdcAtaIx(owner, owner.address), ix]);
}

export async function withdrawCollateral(
  client: AppClient,
  owner: Signer,
  amountRaw: bigint,
): Promise<string> {
  const ownerCollateral = await aaplxAta(owner.address);
  const ix = await getWithdrawCollateralInstructionAsync({
    owner,
    market: MARKET,
    collateralMint: AAPLX_MINT,
    ownerCollateral,
    collateralTokenProgram: TOKEN_2022_PROGRAM,
    amount: amountRaw,
  });
  return send(client, [await ensureAaplxAtaIx(owner, owner.address), ix]);
}

export async function supply(client: AppClient, owner: Signer, amountRaw: bigint): Promise<string> {
  const ownerUsdc = await usdcAta(owner.address);
  const ixs: Instruction[] = [await ensureUsdcAtaIx(owner, owner.address)];
  const existingSupplier = await readSupplier(client.rpc, owner.address);
  if (!existingSupplier) {
    ixs.push(await getOpenSupplierInstructionAsync({ owner, market: MARKET }));
  }
  ixs.push(
    await getSupplyInstructionAsync({
      owner,
      market: MARKET,
      usdcMint: USDC_MINT,
      ownerUsdc,
      usdcTokenProgram: TOKEN_PROGRAM,
      amount: amountRaw,
    }),
  );
  return send(client, ixs);
}

export async function withdrawSupply(
  client: AppClient,
  owner: Signer,
  sharesRaw: bigint,
): Promise<string> {
  const ownerUsdc = await usdcAta(owner.address);
  const ix = await getWithdrawSupplyInstructionAsync({
    owner,
    market: MARKET,
    usdcMint: USDC_MINT,
    ownerUsdc,
    usdcTokenProgram: TOKEN_PROGRAM,
    shares: sharesRaw,
  });
  return send(client, [await ensureUsdcAtaIx(owner, owner.address), ix]);
}

export async function backerDeposit(
  client: AppClient,
  owner: Signer,
  amountRaw: bigint,
): Promise<string> {
  const ownerUsdc = await usdcAta(owner.address);
  const ixs: Instruction[] = [await ensureUsdcAtaIx(owner, owner.address)];
  const existingBacker = await readBacker(client.rpc, owner.address);
  if (!existingBacker) {
    ixs.push(await getOpenBackerInstructionAsync({ owner }));
  }
  ixs.push(
    await getBackerDepositInstructionAsync({
      owner,
      usdcMint: USDC_MINT,
      ownerUsdc,
      usdcTokenProgram: TOKEN_PROGRAM,
      amount: amountRaw,
    }),
  );
  return send(client, ixs);
}

/** Two-step withdraw: request now, finalize after the backstop's withdraw_delay_secs. */
export async function backerRequestWithdraw(
  client: AppClient,
  owner: Signer,
  sharesRaw: bigint,
): Promise<string> {
  const ix = await getRequestWithdrawInstructionAsync({ owner, shares: sharesRaw });
  return send(client, [ix]);
}

/** Owner-only. Defaults to the owner at open_position; call this only to redirect it elsewhere. */
export async function setPayoutAddress(
  client: AppClient,
  owner: Signer,
  payout: Address,
): Promise<string> {
  const [positionPda] = await findPositionPda({ market: MARKET, owner: owner.address });
  const ix = await getSetPayoutAddressInstructionAsync({
    owner,
    position: positionPda,
    payout,
  });
  return send(client, [ix]);
}

/** Fully permissionless -- any wallet may crank this, funds always land at claim.payout. Exposed
 *  here so the payout owner can pull their own streamed payout without depending on anyone else. */
export async function pullClaimPayout(
  client: AppClient,
  payer: Signer,
  claimAddress: Address,
  claimPayout: Address,
): Promise<string> {
  const payoutUsdc = await usdcAta(claimPayout);
  const ix = await getClaimStreamInstructionAsync({
    claim: claimAddress,
    usdcMint: USDC_MINT,
    payoutUsdc,
    usdcTokenProgram: TOKEN_PROGRAM,
  });
  return send(client, [await ensureUsdcAtaIx(payer, claimPayout), ix]);
}

export async function backerFinalizeWithdraw(client: AppClient, owner: Signer): Promise<string> {
  const ownerUsdc = await usdcAta(owner.address);
  const ix = await getFinalizeWithdrawInstructionAsync({
    owner,
    usdcMint: USDC_MINT,
    ownerUsdc,
    usdcTokenProgram: TOKEN_PROGRAM,
  });
  return send(client, [await ensureUsdcAtaIx(owner, owner.address), ix]);
}
