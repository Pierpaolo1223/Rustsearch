// H10: the Worker's response policy, tested without a Worker runtime.
import test from "node:test";
import assert from "node:assert/strict";

import {
  ALLOWED_METHODS,
  DEFAULT_MAX_ARTIFACT_BYTES,
  SECURITY_HEADERS,
  allowedOrigins,
  artifactLimit,
  corsHeaders,
  manifestDigest,
  preflightHeaders,
} from "./headers.js";
import { loadSparseIndex } from "./shard-source.js";

const origin = (value) => ({ headers: { get: (name) => (name === "origin" ? value : null) } });

test("every response carries the security headers", () => {
  assert.equal(SECURITY_HEADERS["x-content-type-options"], "nosniff");
  assert.equal(SECURITY_HEADERS["referrer-policy"], "no-referrer");
  assert.match(SECURITY_HEADERS["content-security-policy"], /default-src 'none'/);
  assert.match(SECURITY_HEADERS["content-security-policy"], /frame-ancestors 'none'/);
  assert.equal(SECURITY_HEADERS["cross-origin-resource-policy"], "same-origin");
  assert.ok(Object.isFrozen(SECURITY_HEADERS));
});

test("CORS is an allowlist, not a wildcard", () => {
  // No configuration: same-origin only, and the response says it varies by origin.
  assert.deepEqual(corsHeaders({}, origin("https://evil.example")), { vary: "origin" });
  // A listed origin is echoed; an unlisted one is not.
  const env = { ALLOWED_ORIGINS: "https://docs.example, https://it.example" };
  assert.deepEqual(corsHeaders(env, origin("https://docs.example")), {
    "access-control-allow-origin": "https://docs.example",
    vary: "origin",
  });
  assert.deepEqual(corsHeaders(env, origin("https://evil.example")), { vary: "origin" });
  // A request with no Origin (curl, a same-origin fetch) gets no CORS header.
  assert.deepEqual(corsHeaders(env, origin("")), { vary: "origin" });
  // The wildcard is opt-in.
  assert.equal(
    corsHeaders({ ALLOWED_ORIGINS: "*" }, origin("https://any.example"))[
      "access-control-allow-origin"
    ],
    "*",
  );
});

test("preflight names the methods and headers only for a listed origin", () => {
  const env = { ALLOWED_ORIGINS: "https://docs.example" };
  assert.deepEqual(preflightHeaders(env, origin("https://evil.example")), { allow: ALLOWED_METHODS });
  const allowed = preflightHeaders(env, origin("https://docs.example"));
  assert.equal(allowed["access-control-allow-methods"], ALLOWED_METHODS);
  assert.equal(allowed["access-control-allow-headers"], "accept, content-type");
  assert.equal(allowed["access-control-max-age"], "86400");
});

test("origins are trimmed and empty entries dropped", () => {
  assert.deepEqual(allowedOrigins({ ALLOWED_ORIGINS: " ,https://a.example , ,https://b.example" }), [
    "https://a.example",
    "https://b.example",
  ]);
  assert.deepEqual(allowedOrigins({}), []);
  assert.deepEqual(allowedOrigins(undefined), []);
});

test("the artifact ceiling defaults and never becomes negative", () => {
  assert.equal(artifactLimit({}), DEFAULT_MAX_ARTIFACT_BYTES);
  assert.equal(artifactLimit({ MAX_ARTIFACT_BYTES: "1024" }), 1024);
  assert.equal(artifactLimit({ MAX_ARTIFACT_BYTES: "-1" }), DEFAULT_MAX_ARTIFACT_BYTES);
  assert.equal(artifactLimit({ MAX_ARTIFACT_BYTES: "not a number" }), DEFAULT_MAX_ARTIFACT_BYTES);
});

test("the manifest digest is normalized, and a malformed pin fails closed", () => {
  assert.equal(manifestDigest({}), undefined, "unset means the per-part hashes only");
  assert.equal(manifestDigest({ MANIFEST_DIGEST: "   " }), undefined);
  const hex = "9f2c".repeat(16);
  // `rustsearch shard` writes `sha256:<hex>`; both spellings are accepted.
  assert.equal(manifestDigest({ MANIFEST_DIGEST: `sha256:${hex.toUpperCase()}` }), hex);
  assert.equal(manifestDigest({ MANIFEST_DIGEST: hex }), hex);
  // A typo must not degrade to "unpinned": it must be impossible to run.
  assert.throws(() => manifestDigest({ MANIFEST_DIGEST: "sha256:abc" }), /64 hex/);
  assert.throws(() => manifestDigest({ MANIFEST_DIGEST: "sha512:" + hex }), /64 hex/);
});

test("a manifest over the artifact budget is refused before any part is fetched", async () => {
  const manifest =
    "format rsx-shard-1\ntotal 4096\nchecksum 0\npart chunks 0 4096 0 chunks\n";
  let reads = 0;
  const bucket = {
    async get() {
      reads += 1;
      return { text: async () => manifest };
    },
  };
  await assert.rejects(
    () => loadSparseIndex(bucket, "index/manifest.txt", { maxBytes: 1024 }),
    /over the 1024 byte limit/,
  );
  assert.equal(reads, 1, "only the manifest was read");
});
