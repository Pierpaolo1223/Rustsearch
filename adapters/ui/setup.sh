#!/usr/bin/env bash
# Build everything the search UI needs to run: the WASM module and a demo index.
#
#   ./setup.sh
#   SEARCHUI_TARGET_DOCS=5000 ./setup.sh
#
# Outputs, all ignored by git:
#   vendor/pkg-web/   wasm-bindgen output (web target)
#   demo/site/        generated HTML corpus plus the tracked curated pages
#   demo/index.rsx    the rustsearch v5 artifact built from it
#   demo/shards/      the same artifact published as a shard set (the default demo)
#
# Re-run it after changing the engine, the corpus or the curated pages; the UI
# itself has no build step.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The playground used to live next to `rustsearch/` (as `searchui/`); it now lives inside
# it, as `rustsearch/adapters/ui/`. Walk up to the workspace instead of assuming a depth,
# so the script works from either layout.
ROOT="$HERE"
while [ "$ROOT" != "/" ] && [ ! -f "$ROOT/rustsearch/Cargo.toml" ]; do
    ROOT="$(dirname "$ROOT")"
done
if [ ! -f "$ROOT/rustsearch/Cargo.toml" ]; then
    echo "cannot find rustsearch/Cargo.toml above $HERE" >&2
    exit 1
fi
RUSTSEARCH="$ROOT/rustsearch"
CURATED_DIR="$HERE/curated"

DOCS="${SEARCHUI_DOCS:-9350}"
MAX_PAGE_CHARS="${SEARCHUI_MAX_PAGE_CHARS:-2117}"
TARGET_DOCS="${SEARCHUI_TARGET_DOCS:-}"

CURATED_PAGES=0
if [ -d "$CURATED_DIR" ]; then
    CURATED_PAGES=$(find "$CURATED_DIR" -maxdepth 1 -name '*.html' | wc -l)
fi

if [ -n "$TARGET_DOCS" ]; then
    case "$TARGET_DOCS" in
        ''|*[!0-9]*)
            echo "SEARCHUI_TARGET_DOCS must be a positive integer" >&2
            exit 2
            ;;
    esac
    if [ "$TARGET_DOCS" -lt 1 ]; then
        echo "SEARCHUI_TARGET_DOCS must be a positive integer" >&2
        exit 2
    fi
fi

if ! command -v wasm-bindgen >/dev/null 2>&1; then
    echo "wasm-bindgen not found; install it with: cargo install wasm-bindgen-cli" >&2
    exit 1
fi
if ! command -v node >/dev/null 2>&1; then
    echo "node is required to generate the demo corpus" >&2
    exit 1
fi

echo "==> building the WASM module"
cd "$RUSTSEARCH"
cargo build -p rustsearch-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir "$HERE/vendor/pkg-web" \
    target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm

echo "==> building the CLI"
cargo build --release -p rustsearch-cli

generate() { # $1 = repository page target
    echo "==> generating up to $1 repository pages"
    # Run from the repository root: the generator indexes every Markdown tree and
    # deliberately skips rustsearch/ and searchui/ so their own docs do not churn
    # the corpus.
    cd "$ROOT"
    node rustsearch/tools/make-corpus.mjs "$HERE/demo/site" \
        --target "$1" --chunk 850 --facets
    if [ "$CURATED_PAGES" -gt 0 ]; then
        echo "==> adding $CURATED_PAGES curated demo pages"
        cp "$CURATED_DIR"/*.html "$HERE/demo/site/"
    fi
}

build_index() { # $1 = max-page-chars, $2 = output artifact
    # --base-url demo/site makes every hit's URL open the generated page when the
    # UI is served from searchui/ (demo/site/doc-00042.html). The 64 KiB term
    # chunks make this a format-v5 artifact; setup publishes it as a shard set
    # below, which is what the page loads by default.
    "$RUSTSEARCH/target/release/rustsearch" build "$HERE/demo/site" \
        -o "$2" --base-url demo/site --stemmer en --max-page-chars "$1" \
        --term-chunk-bytes 64k
}

doc_count() { # $1 = artifact
    "$RUSTSEARCH/target/release/rustsearch" info "$1" |
        awk '/^documents:/{print $2}'
}

# The repository pages plus the curated pages make up the demo site.
GENERATED=$((DOCS - CURATED_PAGES))
if [ "$GENERATED" -lt 1 ]; then
    GENERATED=1
fi

if [ -n "$TARGET_DOCS" ]; then
    generate "$GENERATED"
    PAGES=$(find "$HERE/demo/site" -maxdepth 1 -name '*.html' | wc -l)

    if [ "$TARGET_DOCS" -lt "$PAGES" ]; then
        # Fewer documents than pages: keep only enough repository pages to hit
        # the target and skip splitting entirely.
        KEEP=$((TARGET_DOCS - CURATED_PAGES))
        if [ "$KEEP" -lt 1 ]; then
            KEEP=1
        fi
        generate "$KEEP"
        build_index 0 "$HERE/demo/index.rsx"
    else
        echo "==> searching for a split threshold that yields exactly $TARGET_DOCS documents"
        TMP="$(mktemp -d)"
        trap 'rm -rf "$TMP"' EXIT
        BEST=""
        LOW=1
        HIGH=1000000
        while [ "$LOW" -le "$HIGH" ]; do
            MID=$(( (LOW + HIGH) / 2 ))
            build_index "$MID" "$TMP/probe.rsx" >/dev/null
            COUNT="$(doc_count "$TMP/probe.rsx")"
            if [ "$COUNT" -eq "$TARGET_DOCS" ]; then
                BEST="$MID"
                break
            fi
            if [ "$COUNT" -gt "$TARGET_DOCS" ]; then
                LOW=$((MID + 1))
            else
                HIGH=$((MID - 1))
            fi
        done
        if [ -z "$BEST" ]; then
            echo "cannot reach $TARGET_DOCS documents: the corpus has $PAGES pages; pick a target of at least $PAGES or set SEARCHUI_MAX_PAGE_CHARS manually" >&2
            exit 1
        fi
        build_index "$BEST" "$HERE/demo/index.rsx"
    fi
else
    generate "$GENERATED"
    # The repository currently yields ~9.3k pages; splitting pages above ~2117
    # bytes puts the index at 10,000 documents while leaving long pages for
    # "Group by page".
    build_index "$MAX_PAGE_CHARS" "$HERE/demo/index.rsx"
fi

echo "==> publishing the default shard set in demo/shards"
rm -rf "$HERE/demo/shards"
"$RUSTSEARCH/target/release/rustsearch" shard "$HERE/demo/index.rsx" \
    -o "$HERE/demo/shards" --prefix index/ >/dev/null
"$RUSTSEARCH/target/release/rustsearch" info "$HERE/demo/index.rsx"
printf 'chunks:      %s\n' "$(find "$HERE/demo/shards" -maxdepth 1 -name 'chunk*.bin' | wc -l)"

printf '\nReady. Start the UI with:\n  %s/serve.sh\n' "$HERE"
