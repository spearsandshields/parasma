#!/usr/bin/env bash
#
# build-wasm.sh — the one wasm-pack invocation. Every build of the shipped
# binary goes through here so the path remap below is never forgotten.
#
# rustc embeds the source path of every panic site in the binary (the
# core::panic::Location table). For workspace crates that path is already
# relative, but for registry crates and git checkouts it is absolute, so a
# plain build ships the builder's home directory and cargo layout inside the
# wasm. The remap replaces those prefixes with stable placeholders; the file
# name and line survive, which is all the panic hook needs.
#
# Usage:  ./scripts/build-wasm.sh [--out-dir DIR] [-- <cargo args>]
#         (the default out-dir is what packages/parasma-core imports)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT_DIR="../../packages/parasma-core/src/wasm"
if [ "${1:-}" = "--out-dir" ]; then
  OUT_DIR="$2"; shift 2
fi

CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
# Later entries win when prefixes overlap, so the broad $HOME rule comes
# first and the specific ones after it.
REMAP="--remap-path-prefix=$HOME=/home"
REMAP="$REMAP --remap-path-prefix=$CARGO_HOME_DIR=/cargo"
REMAP="$REMAP --remap-path-prefix=$ROOT=/parasma"
export RUSTFLAGS="${RUSTFLAGS:-} $REMAP"

exec wasm-pack build crates/parasma-wasm --target web --out-dir "$OUT_DIR" "$@"
