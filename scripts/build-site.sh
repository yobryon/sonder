#!/usr/bin/env bash
# Build the serverless browser version of Sonder into ./site (GitHub Pages friendly).
#
#   scripts/build-site.sh            # release wasm + bindings + static files → site/
#   python3 -m http.server -d site   # then open http://localhost:8000/
#
# Env: CARGO_TARGET_DIR (default target/web), WASM_OPT=0 to skip wasm-opt even if installed.
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/web}"
TRIPLE=wasm32-unknown-unknown
OUT=site

if ! rustup target list --installed 2>/dev/null | grep -q "^$TRIPLE$"; then
  echo "+ rustup target add $TRIPLE"
  rustup target add "$TRIPLE"
fi
if ! command -v wasm-bindgen >/dev/null; then
  echo "wasm-bindgen CLI not found. Install the version matching Cargo.lock:" >&2
  echo "  cargo install wasm-bindgen-cli --version $(scripts/wasm-bindgen-version.sh)" >&2
  exit 1
fi
WANT="$(scripts/wasm-bindgen-version.sh)"
HAVE="$(wasm-bindgen --version | awk '{print $2}')"
if [ "$WANT" != "$HAVE" ]; then
  echo "warning: wasm-bindgen CLI $HAVE != crate $WANT in Cargo.lock; bindings may fail to load" >&2
fi

echo "+ cargo build --release --target $TRIPLE -p sonder-wasm"
cargo build --release --target "$TRIPLE" -p sonder-wasm

rm -rf "$OUT"
mkdir -p "$OUT/pkg"
echo "+ wasm-bindgen → $OUT/pkg"
wasm-bindgen --target web --no-typescript --out-dir "$OUT/pkg" "$CARGO_TARGET_DIR/$TRIPLE/release/sonder_wasm.wasm"

if [ "${WASM_OPT:-1}" != "0" ] && command -v wasm-opt >/dev/null; then
  echo "+ wasm-opt -Oz"
  # wasm-bindgen emits reference types (an externref table its JS glue grows at startup);
  # an old wasm-opt without this flag freezes the table → "Table.grow(): failed to grow".
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-reference-types -o "$OUT/pkg/sonder_wasm_bg.wasm.opt" "$OUT/pkg/sonder_wasm_bg.wasm" \
    && mv "$OUT/pkg/sonder_wasm_bg.wasm.opt" "$OUT/pkg/sonder_wasm_bg.wasm" \
    || echo "warning: wasm-opt failed; keeping the unoptimised module" >&2
fi

echo "+ copy static files"
cp -r crates/sonder-web/static/. "$OUT/"
touch "$OUT/.nojekyll"

du -sh "$OUT/pkg/sonder_wasm_bg.wasm" | sed 's/^/wasm: /'
echo "site built in $OUT/ — serve with: python3 -m http.server -d $OUT"
