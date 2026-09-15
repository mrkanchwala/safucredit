/**
 * Turns a raw wallet/transaction error into something a user can read.
 *
 * The raw shape is nasty by design: @solana/kit strips human-readable error
 * context out of production bundles to save bundle size, so what actually
 * reaches the UI is:
 *   "Solana error #11; Decode this error by running `npx @solana/errors
 *    decode -- 11 '<base64>'`"
 * where <base64> decodes to a URL-encoded query string. Its `causeMessage`
 * field is very often ANOTHER one of these same decode-instruction strings,
 * one level deeper (a Codama codec error wrapping the real transaction
 * simulation error) -- so this unwraps recursively. At the bottom, the
 * `logs` field is the real simulation log array, and Anchor's own runtime
 * already writes a plain-English line into it for every custom program
 * error:
 *   "Program log: AnchorError thrown in programs/backstop/src/lib.rs:402.
 *    Error Code: PausedForInventory. Error Number: 6032. Error Message:
 *    Deposits and withdrawals pause while the pool holds inventory."
 * That "Error Message: ..." text is already exactly what a user should see,
 * word for word -- no hand-maintained duplicate message list to keep in
 * sync with the IDL, so it can't go stale the way a hardcoded map would.
 *
 * Action hints are keyed by the Rust error-code NAME, not the numeric Error
 * Number -- stock_vault and backstop are two separate programs whose
 * numeric ranges overlap (both start at 6000), so the same number means two
 * different things depending on which program actually threw (e.g. 6008 is
 * PriceUnavailable in stock_vault but VerdictDeadlineTooFar in backstop).
 * Verified against both program error name lists that none of the names
 * used as keys below collide with a different meaning in the other program.
 */

const ACTION_HINTS: Record<string, string> = {
  PriceUnavailable: "The price feed needs a fresh push before this will go through. Try again in a moment.",
  StalePriceUpdate: "The price feed needs a fresh push before this will go through. Try again in a moment.",
  PausedForInventory: "The backstop pool is mid-sale on purchased inventory. Deposits and withdrawals reopen once that clears.",
  ExceedsLtv: "Lower the amount, or add more collateral first.",
  InsufficientCash: "The market doesn't have enough USDC on hand right now. Try a smaller amount.",
  InsufficientBalance: "Not enough balance for this amount.",
  CapReached: "The market has hit its borrow or collateral cap. Try a smaller amount.",
  NotLiquidatable: "This position isn't past its liquidation threshold, so it can't be liquidated yet.",
  CorporateActionHold: "Paused around a stock split. This clears once the price is confirmed at the new ratio.",
  NoWithdrawalPending: "There's no withdrawal queued to finalize yet.",
  WithdrawalNotReady: "The withdrawal delay hasn't elapsed yet. Check back later.",
  CooldownNotElapsed: "The cooldown period hasn't elapsed yet. Check back later.",
  GateNotElapsed: "The waiting period on this claim hasn't elapsed yet.",
};

export type FriendlyError = {
  /** Plain-English sentence, safe to show as the primary message. */
  message: string;
  /** Optional one-line "what to do next", shown under the message when known. */
  action?: string;
  /** The original raw error text, for a collapsed "technical details" toggle. */
  raw: string;
};

const DECODE_INSTRUCTION_RE = /decode -- \d+ '([A-Za-z0-9+/=_-]+)'/;
const ANCHOR_ERROR_RE =
  /AnchorError thrown in [^:]+:\d+\. Error Code: (\w+)\. Error Number: \d+\. Error Message: ([\s\S]+?)(?:, Program |\.\s*$|$)/;

function base64Decode(b64: string): string {
  try {
    if (typeof atob === "function") return atob(b64);
  } catch {
    // fall through
  }
  // Node (tests, SSR) has no global atob in older runtimes.
  return Buffer.from(b64, "base64").toString("utf-8");
}

/** Unwraps one or more nested "Solana error #N; ... '<base64>'" layers and
 *  returns the innermost text that actually contains simulation content
 *  (an Anchor error message, or a percent-encoded `logs` field). */
function unwrap(text: string, depthRemaining = 4): string {
  const b64Match = text.match(DECODE_INSTRUCTION_RE);
  if (!b64Match || depthRemaining <= 0) return text;

  let queryString: string;
  try {
    queryString = base64Decode(b64Match[1]);
  } catch {
    return text;
  }

  let decoded: string;
  try {
    decoded = decodeURIComponent(queryString);
  } catch {
    decoded = queryString;
  }

  // The decoded query string's `causeMessage` or `logs` field is itself
  // percent-encoded content (it was encoded before being placed in the
  // outer query string) -- if it's ANOTHER decode-instruction string,
  // recurse; otherwise this is the payload we want.
  if (DECODE_INSTRUCTION_RE.test(decoded)) {
    return unwrap(decoded, depthRemaining - 1);
  }
  return decoded;
}

export function toFriendlyError(error: unknown): FriendlyError {
  const raw = error instanceof Error ? error.message : String(error);
  const unwrapped = unwrap(raw);

  const match = unwrapped.match(ANCHOR_ERROR_RE);
  if (match) {
    const [, codeName, messageRaw] = match;
    const message = messageRaw.replace(/\.$/, "").trim();
    return {
      message: message.charAt(0).toUpperCase() + message.slice(1) + ".",
      action: ACTION_HINTS[codeName],
      raw,
    };
  }

  // Wallet-level rejections already read fine to a user -- pass through
  // rather than wrapping in "technical details".
  if (/user rejected|reject.*request|declined/i.test(raw)) {
    return { message: "Request was declined in the wallet.", raw };
  }

  if (/insufficient.*(lamports|sol)/i.test(unwrapped)) {
    return { message: "Not enough SOL in the wallet to cover network fees.", raw };
  }

  // No recognizable program error and no known pattern -- generic fallback,
  // never the raw Codama/decode-instruction text.
  return {
    message: "This transaction didn't go through. Try again in a moment.",
    raw,
  };
}
