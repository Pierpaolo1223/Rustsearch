#!/usr/bin/env bash
# Serve the UI over HTTP. The page fetches the WASM module and the artifact, which
# a file:// URL cannot do.
#
#   ./serve.sh [port]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT="${1:-8765}"

if [[ ! -f "$HERE/demo/shards/manifest.txt" ]]; then
    echo "demo/shards/manifest.txt is missing; run $HERE/setup.sh first" >&2
    exit 1
fi

echo "rustsearch search playground: http://127.0.0.1:$PORT/"
# serve.py is this directory's server, not the stdlib one: it answers `Range`
# requests, which a sharded index (?shards=...) reads a few hundred bytes with.
exec python3 "$HERE/serve.py" "$HERE" "$PORT"
