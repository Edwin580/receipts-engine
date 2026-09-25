#!/usr/bin/env bash
# Builds the engine for the browser (ADR 0005):
#   web/public/pkg/st  single-threaded, stable Rust (works everywhere)
#   web/public/pkg/mt  multi-threaded, pinned nightly + build-std (needs
#               cross-origin isolation: COOP/COEP headers)
# Requires: rustup targets/toolchains below and wasm-bindgen-cli matching
# the wasm-bindgen version pinned in Cargo.toml.
set -euo pipefail
cd "$(dirname "$0")/../.."

NIGHTLY=nightly-2026-09-20
WB_VERSION=$(sed -n 's/^wasm-bindgen = "=\(.*\)"/\1/p' Cargo.toml)
if [ "$(wasm-bindgen --version | cut -d' ' -f2)" != "$WB_VERSION" ]; then
  echo "wasm-bindgen-cli $WB_VERSION required: cargo install wasm-bindgen-cli --version $WB_VERSION --locked" >&2
  exit 1
fi

what=${1:-all}

if [ "$what" = all ] || [ "$what" = st ]; then
  cargo build --release --target wasm32-unknown-unknown -p receipts-wasm
  wasm-bindgen --target web --out-dir web/public/pkg/st \
    target/wasm32-unknown-unknown/release/receipts_wasm.wasm
fi

if [ "$what" = all ] || [ "$what" = mt ]; then
  # Atomics alone no longer make the linker emit shared memory: ask for it
  # (and the TLS exports wasm-bindgen's thread setup needs) explicitly.
  RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--max-memory=4294967296 -C link-arg=--import-memory -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base' \
    cargo +"$NIGHTLY" build --release --target wasm32-unknown-unknown \
      --target-dir target/wasm-mt -p receipts-wasm --features parallel \
      -Z build-std=panic_abort,std
  wasm-bindgen --target web --out-dir web/public/pkg/mt \
    target/wasm-mt/wasm32-unknown-unknown/release/receipts_wasm.wasm
  # wasm-bindgen-rayon's worker imports the package as `../../..`, which
  # only bundlers resolve. Point it at the module file so the package works
  # as plain static files (Cloudflare Pages, the dev servers).
  sed -i "s#import('../../..')#import('../../../receipts_wasm.js')#" \
    web/public/pkg/mt/snippets/wasm-bindgen-rayon-*/src/workerHelpers.js
  grep -q "import('../../../receipts_wasm.js')" web/public/pkg/mt/snippets/wasm-bindgen-rayon-*/src/workerHelpers.js
fi
ls -la web/public/pkg/*/receipts_wasm_bg.wasm
