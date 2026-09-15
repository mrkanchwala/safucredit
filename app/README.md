# SAFU Credit App

The dapp served at [credit.safustaking.com](https://credit.safustaking.com). React + [`@solana/kit`](https://github.com/anza-xyz/kit), talking directly to the `stock_vault` and `backstop` programs in `../solana/programs/`.

See the [repository README](../README.md) for what this product does, the liquidation guarantees, and how to run the tests.

## Local dev

```bash
npm install
npm run dev      # vite dev server
npm run build     # tsc -b && vite build
npm run lint      # oxlint
```

`codegen.mjs` regenerates the Codama-derived TypeScript client bindings from `../solana/idl/` after a program change.
