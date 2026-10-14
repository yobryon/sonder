#!/usr/bin/env bash
# Print the wasm-bindgen crate version pinned in Cargo.lock (the CLI must match it exactly).
set -euo pipefail
cd "$(dirname "$0")/.."
awk '$0=="name = \"wasm-bindgen\"" {f=1; next} f && /^version/ {gsub(/"/,"",$3); print $3; exit}' Cargo.lock
