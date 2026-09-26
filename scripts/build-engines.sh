#!/usr/bin/env bash
# Build translator-core engine packages for macOS (Apple Silicon first).
#
# Usage: scripts/build-engines.sh [cpu|blas|metal] [--test]
#
# Metal needs no extra SDK beyond Xcode command line tools: whisper-rs-sys
# compiles GGML_METAL with the shader library embedded, so the package is the
# binary alone. cpu/blas follow the same layout as build-engines.ps1.
set -euo pipefail

engine="${1:-metal}"
test_flag="${2:-}"

case "$engine" in
  cpu|blas|metal) ;;
  *) echo "unknown engine: $engine (expected cpu|blas|metal)" >&2; exit 1 ;;
esac

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

if [[ "$engine" == "cpu" ]]; then
  features="whisper"
else
  features="$engine"
fi

cargo build -p translator-core --release --no-default-features --features "$features"

dest="$repo/engines/$engine"
mkdir -p "$dest"
cp "$repo/target/release/translator-core" "$dest/"

actual="$("$dest/translator-core" --engine-info)"
if [[ "$actual" != "$engine" ]]; then
  echo "Engine verification failed: $engine ($actual)" >&2
  exit 1
fi

if [[ "$test_flag" == "--test" ]]; then
  cargo test -p translator-core --release --no-default-features --features "$features"
fi

echo "Engine package ready: $dest"
