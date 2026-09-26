#!/usr/bin/env bash
# Build the macOS Swift translation bridge (clients/macos/translator-bridge).
#
# Usage: scripts/build-bridge.sh
# Output: target/translator-bridge
#
# The bridge hosts the Apple Translation framework and serves `apple-translate`
# sessions on /tmp/translator-bridge-v1.sock (see translator-core's apple.rs).
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

src="$repo/clients/macos/translator-bridge/main.swift"
dest="$repo/target/translator-bridge"
mkdir -p "$(dirname "$dest")"

# -swift-version 5: keep the AppKit/SwiftUI mixing free of strict-concurrency
# errors; the bridge is single-client by contract.
swiftc -O -swift-version 5 "$src" -o "$dest"

echo "bridge built: $dest"
