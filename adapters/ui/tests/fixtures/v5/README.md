# v5 end-to-end fixture

A real, deliberately tiny format-v5 artifact and its shard set, used by
`tests/e2e-v5.test.mjs`. 30 generated HTML pages (one document per page), about
14,500 body tokens, 61 terms. Built with a 1 KiB term budget so the dictionary is
three chunks, and with 30 records so the record table is one page; `texts` is
19,755 of the 26,801 bytes, which is what makes the per-hit range reads visible in
the test.

Rebuild it from the repository root (the CLI and `wasm/pkg-node` must exist):

```sh
node - <<'EOF'
import { mkdirSync, writeFileSync } from "node:fs";
mkdirSync("/tmp/e2e-site", { recursive: true });
const words = ["walkdir","postgres","security","index","worker","cache","shard","query","ranking","artifact","token","config","server","client","benchmark","error","test","database","deploy","preview"];
for (let page = 0; page < 30; page += 1) {
  const topic = words[page % words.length];
  const sentences = [];
  for (let s = 0; s < 30; s += 1) {
    const a = words[(page + s) % words.length];
    const b = words[(page * 3 + s * 7) % words.length];
    sentences.push(`<p>Page ${page} sentence ${s}: the ${topic} ${a} keeps the ${b} small and every example in the guide shows the same three calls in a row.</p>`);
  }
  writeFileSync(
    `/tmp/e2e-site/page${String(page).padStart(2, "0")}.html`,
    `<html lang="en"><head><title>Guide ${page} — ${topic}</title><meta name="keywords" content="guide, ${topic}"></head><body><main><h1>Guide ${page}: ${topic}</h1>${sentences.join("")}</main></body></html>`,
  );
}
EOF

rustsearch/target/release/rustsearch build /tmp/e2e-site \
  -o searchui/tests/fixtures/v5/index.rsx --stemmer en \
  --term-chunk-bytes 1024 --base-url /site
rustsearch/target/release/rustsearch shard searchui/tests/fixtures/v5/index.rsx \
  -o searchui/tests/fixtures/v5/shards --prefix index/
```

The artifact is immutable, so the test needs only the two directories above.
