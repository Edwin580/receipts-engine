# Receipts web app

React + TypeScript (Vite). The engine runs in a Web Worker as WebAssembly.
It uses the threaded build when the page is cross-origin isolated, and the
single-threaded build otherwise (`src/engine/worker.ts`).

## Run locally

```sh
tools/wasm/build.sh                     # from the repo root: web/public/pkg/{st,mt}
cd web && npm ci
RECEIPTS_SNAPSHOT_DIR=../snapshots/nyc311/<hash16> npm run dev
```

`RECEIPTS_SNAPSHOT_DIR` is served at `/snap/`. A small synthetic snapshot
works too (`receipts-snapshot synth` then `build`).

## What's where

| Path | |
|---|---|
| `src/plans.ts` | Every question the app asks, as a plan (`docs/engine/plan.md`), plus the known-issue filters for what-ifs. |
| `src/App.tsx` | Loading and verification, questions, the answer, and exclusion state. |
| `src/components/PipelineView.tsx` | Each step in plain English with row counts, and the rows behind the selected number. |
| `src/components/ReceiptPanel.tsx` | The records behind a number, "can one record change it?", what-ifs, and copying the receipt (hashes + plan). |
| `src/engine/` | The worker and its typed client. |
| `public/_headers` | COOP/COEP for Cloudflare Pages (`docs/deploy.md`). |
| `engine-test/` | Engine-only benchmark and parity tools (M4). |

## Tests

```sh
npm test                                              # unit tests
RECEIPTS_SNAPSHOT_DIR=<snapshot dir> npm test         # + every plan run in the real engine
RECEIPTS_SNAPSHOT_DIR=<snapshot dir> npm run e2e      # Playwright, against the production build
```

With a snapshot, the unit tests also run every question and year in the
real WASM engine. They check that each known-issue filter selects exactly
as many rows as the snapshot builder counted.
