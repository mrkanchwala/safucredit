type Action = {
  isError: boolean;
  isSuccess?: boolean;
  error: unknown;
  data: unknown;
};

export function TxStatus({ action }: { action: Action }) {
  if (action.isError) {
    const msg = action.error instanceof Error ? action.error.message : String(action.error);
    return <div className="tx-status error">{msg}</div>;
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
