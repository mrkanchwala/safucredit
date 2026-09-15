import { createClient } from "@solana/kit";
import { solanaDevnetRpc } from "@solana/kit-plugin-rpc";
import { walletSigner } from "@solana/kit-plugin-wallet";
import { RPC_URL } from "./market";

// Wallet scope LOCKED 2026-09-15 (founder, /plan-eng-review): Phantom, Solflare, Backpack only,
// extension-only. WalletConnect explicitly dropped -- do not add it back without a founder decision.
export const ALLOWED_WALLET_NAMES = ["Phantom", "Solflare", "Backpack"] as const;

// RPC_URL is the single named override point (market.ts) -- currently the public devnet endpoint,
// no fallback. Swap this one value for a dedicated provider before mainnet; the plugin already
// supports it via rpcUrl, no rework needed (CIE review 2026-09-15).
export const client = createClient()
  .use(walletSigner({ chain: "solana:devnet" }))
  .use(solanaDevnetRpc({ rpcUrl: RPC_URL }));

export type AppClient = Awaited<typeof client>;
