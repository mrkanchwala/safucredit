import { address, type Address } from "@solana/kit";
import devnetMarket from "../devnet-market.json";

// Single AAPLx market on devnet. Created 2026-09-15 by scripts/setup_devnet_market.py against the
// live deployed programs -- see memory/projects/safu/stocklana.md "Keys" for the admin key that owns
// mint/pause authority on the mock AAPLx mint.
export const CLUSTER: "devnet" = "devnet";
export const RPC_URL = "https://api.devnet.solana.com";

export const STOCK_VAULT_PROGRAM: Address = address(devnetMarket.stockVaultProgram);
export const BACKSTOP_PROGRAM: Address = address(devnetMarket.backstopProgram);
export const AAPLX_MINT: Address = address(devnetMarket.aaplxMint);
export const USDC_MINT: Address = address(devnetMarket.usdcMint);
export const MARKET: Address = address(devnetMarket.market);
export const COLLATERAL_VAULT: Address = address(devnetMarket.collateralVault);
export const USDC_VAULT: Address = address(devnetMarket.usdcVault);

export const AAPLX_DECIMALS = 8;
export const USDC_DECIMALS = 6;

export const TOKEN_2022_PROGRAM: Address = address(
  "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
);
export const TOKEN_PROGRAM: Address = address(
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
);

// Locked spec, matches scripts/demo.py MARKET_PARAMS / scripts/setup_devnet_market.py.
export const MAX_LTV_BPS = 4_000;
export const LIQ_THRESHOLD_BPS = 5_000;
