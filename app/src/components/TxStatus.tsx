import { toFriendlyError } from "../lib/friendly-error";

type Action = {
  isError: boolean;
  isSuccess?: boolean;
  error: unknown;
  data: unknown;
};

export function TxStatus({ action }: { action: Action }) {
  if (action.isError) {
    const friendly = toFriendlyError(action.error);
    return (
      <div className="tx-status error">
        <div>{friendly.message}</div>
        {friendly.action ? <div className="tx-status-hint">{friendly.action}</div> : null}
        <details className="tx-status-details">
          <summary>Technical details</summary>
          <div>{friendly.raw}</div>
        </details>
      </div>
    );
  }
  if (action.data && typeof action.data === "string") {
    return (
      <div className="tx-status">
        <a href={`https://explorer.solana.com/tx/${action.data}?cluster=devnet`} target="_blank" rel="noreferrer">
          view on explorer ↗
        </a>
      </div>
    );
  }
  return null;
}
