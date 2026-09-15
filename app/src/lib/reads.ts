import type { Address } from "@solana/kit";
import type { AppClient } from "./client";
import {
  fetchMaybeMarket,
  fetchMaybePosition,
  fetchMaybeSupplier,
  findPositionPda,
  findSupplierPda,
} from "../generated/stock_vault";
import {
  fetchMaybeBacker,
  fetchMaybeBackstopConfig,
  fetchMaybeBorrowerClaims,
  fetchMaybeClaim,
  fetchMaybeInventory,
  findBackerPda,
  findBorrowerClaimsPda,
  findClaimPda,
  findConfigPda,
  findInventoryPda,
  ClaimStatus,
} from "../generated/backstop";
import { AAPLX_DECIMALS, MARKET, USDC_DECIMALS } from "./market";
import { aaplxAta, usdcAta } from "./ata";

export { ClaimStatus };

type RpcClient = AppClient["rpc"];

export async function readTokenBalance(rpc: RpcClient, tokenAccount: Address): Promise<bigint> {
  try {
    const res = await rpc.getTokenAccountBalance(tokenAccount).send();
    return BigInt(res.value.amount);
  } catch {
    return 0n; // ATA not created yet -- treated as zero, same as any fresh wallet.
  }
}

export async function readAaplxBalance(rpc: RpcClient, owner: Address): Promise<bigint> {
  const ata = await aaplxAta(owner);
  return readTokenBalance(rpc, ata);
}

export async function readUsdcBalance(rpc: RpcClient, owner: Address): Promise<bigint> {
  const ata = await usdcAta(owner);
  return readTokenBalance(rpc, ata);
}

export async function readMarket(rpc: RpcClient) {
  const acc = await fetchMaybeMarket(rpc, MARKET);
  if (!acc.exists) return null;
  return acc.data;
}

export async function readPosition(rpc: RpcClient, owner: Address) {
  const [positionPda] = await findPositionPda({ market: MARKET, owner });
  const acc = await fetchMaybePosition(rpc, positionPda);
  return acc.exists ? acc.data : null;
}

export async function readSupplier(rpc: RpcClient, owner: Address) {
  const [supplierPda] = await findSupplierPda({ market: MARKET, owner });
  const acc = await fetchMaybeSupplier(rpc, supplierPda);
  return acc.exists ? acc.data : null;
}

export async function readBacker(rpc: RpcClient, owner: Address) {
  const [backerPda] = await findBackerPda({ owner });
  const acc = await fetchMaybeBacker(rpc, backerPda);
  return acc.exists ? acc.data : null;
}

const ZERO_ADDRESS = "11111111111111111111111111111111" as Address;

/** Seized collateral the backstop pool is holding from a liquidation, still to be resold.
 *  Deposits/withdrawals pause on the pool while this is non-zero (see PausedForInventory). */
export async function readInventory(rpc: RpcClient) {
  const [inventoryPda] = await findInventoryPda({ market: MARKET });
  const acc = await fetchMaybeInventory(rpc, inventoryPda);
  return acc.exists ? { address: inventoryPda, data: acc.data } : null;
}

export async function readBackstopConfig(rpc: RpcClient) {
  const [configPda] = await findConfigPda();
  const acc = await fetchMaybeBackstopConfig(rpc, configPda);
  return acc.exists ? acc.data : null;
}

/** Returns null when the borrower has never had a claim on this market. */
export async function readActiveClaim(rpc: RpcClient, owner: Address) {
  const [borrowerClaimsPda] = await findBorrowerClaimsPda({ market: MARKET, borrower: owner });
  const bc = await fetchMaybeBorrowerClaims(rpc, borrowerClaimsPda);
  if (!bc.exists || bc.data.open === ZERO_ADDRESS) return null;
  const [claimPda] = await findClaimPda({ liquidationRecord: bc.data.open });
  const claim = await fetchMaybeClaim(rpc, claimPda);
  return claim.exists ? { address: claimPda, data: claim.data } : null;
}

export function fromRaw(raw: bigint | number, decimals: number): number {
  return Number(raw) / 10 ** decimals;
}

export function toRaw(ui: number, decimals: number): bigint {
  return BigInt(Math.round(ui * 10 ** decimals));
}

export const fmtAaplx = (raw: bigint) => fromRaw(raw, AAPLX_DECIMALS).toFixed(4);
export const fmtUsdc = (raw: bigint) => fromRaw(raw, USDC_DECIMALS).toFixed(2);
/** Backer shares are minted 1:1 with raw USDC units on first deposit (backstop/src/lib.rs
 *  `deposit`: `shares = amount as u128` when `total_shares == 0`) and only drift from that with
 *  pool P&L thereafter, so the same 6-decimal scaling as USDC applies -- this is a display fix,
 *  not a claim that 1 share always equals exactly $1. */
export const fmtShares = (raw: bigint) => fromRaw(raw, USDC_DECIMALS).toFixed(2);

/** safu-core::lending::INDEX_SCALE (crates/safu-core/src/lending.rs:8) -- verified from source, not guessed. */
const INDEX_SCALE = 1_000_000_000_000n;

/** Mirrors safu_core::lending::debt_for_shares -- ceil(shares * index / INDEX_SCALE). */
export function debtForShares(shares: bigint, borrowIndex: bigint): bigint {
  const num = shares * borrowIndex;
  return num === 0n ? 0n : (num + INDEX_SCALE - 1n) / INDEX_SCALE;
}

/** Mirrors safu_core::collateral::collateral_value (crates/safu-core/src/collateral.rs:29), for
 *  multiplier_fp == MULT_SCALE (no active stock split -- true for this market today). General
 *  form: floor(raw * price_fp * multiplier_fp / (MULT_SCALE * 10^(PRICE_DECIMALS-USD_DECIMALS) *
 *  10^AAPLX_DECIMALS)); with multiplier_fp == MULT_SCALE the MULT_SCALE terms cancel, leaving
 *  floor(raw * price_fp / 10^(PRICE_DECIMALS-USD_DECIMALS+AAPLX_DECIMALS)) = floor(raw * price_fp
 *  / 1e10) for PRICE_DECIMALS=8, USD_DECIMALS=6, AAPLX_DECIMALS=8. Verified: 10 AAPLx at $330 ->
 *  collateral_value = 1_000_000_000 * 33_000_000_000 / 1e10 = 3_300_000_000 (raw USDC, $3,300). */
export function collateralValueRaw(rawCollateral: bigint, priceLast: bigint): bigint {
  return (rawCollateral * priceLast) / 10_000_000_000n;
}

/** Mirrors safu_core::lending::max_borrow -- collateral_value * ltv_bps / 10_000, floor. A 0.1%
 *  safety margin is subtracted so a MAX-borrow doesn't get rejected by ExceedsLtv if a sliver of
 *  interest accrues on existing debt between this read and the transaction landing -- the same
 *  class of timing gap that can leave a "repay everything shown" transaction a fraction of a cent
 *  short, found live 2026-09-15. */
export function maxBorrowRaw(collateralValue: bigint, ltvBps: number): bigint {
  const raw = (collateralValue * BigInt(ltvBps)) / 10_000n;
  return (raw * 999n) / 1_000n;
}

function totalBorrows(market: { totalBorrowShares: bigint; borrowIndex: bigint }): bigint {
  return debtForShares(market.totalBorrowShares, market.borrowIndex);
}

/** Mirrors logic::total_assets -- cash + total_borrows - backer_interest_owed. */
export function totalAssets(market: {
  cash: bigint;
  totalBorrowShares: bigint;
  borrowIndex: bigint;
  backerInterestOwed: bigint;
}): bigint {
  return market.cash + totalBorrows(market) - market.backerInterestOwed;
}

/** A supplier's current USDC-equivalent value: shares * total_assets / total_supply_shares, floor. */
export function supplierValueRaw(
  market: {
    cash: bigint;
    totalBorrowShares: bigint;
    borrowIndex: bigint;
    backerInterestOwed: bigint;
    totalSupplyShares: bigint;
  },
  supplierShares: bigint,
): bigint {
  if (market.totalSupplyShares === 0n) return 0n;
  return (supplierShares * totalAssets(market)) / market.totalSupplyShares;
}

/** Market.price.last_price is a u64 scaled by 1e8 (see scripts/demo.py PRICE_SCALE). */
export const PRICE_SCALE = 100_000_000n;
export function fmtPrice(lastPrice: bigint): string {
  return (Number(lastPrice) / Number(PRICE_SCALE)).toFixed(2);
}
