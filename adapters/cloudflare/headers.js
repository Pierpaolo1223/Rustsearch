// Response policy for the search Worker (H10).
//
// The Worker's answer is JSON, but a browser that navigates to it, an `<iframe>`
// that embeds it, or a page on another origin that fetches it all see the same
// response. The headers below close those three: `default-src 'none'` means the
// response cannot load anything even if a client renders it as a document, the
// frame policy refuses embedding, and CORS is an allowlist from the environment
// instead of `*` — with no `ALLOWED_ORIGINS` set, only same-origin callers get a
// readable answer.
//
// Kept in its own module, free of `wasm` and `R2`, so Node can test the policy
// without a Worker runtime.

/** Sent on every response, including errors: the JSON is the whole body. */
export const SECURITY_HEADERS = Object.freeze({
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "content-security-policy": "default-src 'none'; frame-ancestors 'none'; base-uri 'none'",
  "cross-origin-resource-policy": "same-origin",
});

/** The methods the endpoint answers. Everything else is 405, not a silent GET. */
export const ALLOWED_METHODS = "GET, HEAD, OPTIONS";

/** 256 MiB: larger than the 1M-document artifact (737 MB) is not, but an
 * accidental upload of a whole bucket is refused before the engine copies it. */
export const DEFAULT_MAX_ARTIFACT_BYTES = 256 * 1024 * 1024;

/**
 * One pinned SHA-256 from the environment, or `undefined` when it is unset.
 *
 * `rustsearch shard` writes `sha256:<hex>` sidecars, so the prefix is accepted and
 * stripped. A malformed value throws: a typo must not degrade to "unpinned".
 */
function pinnedDigest(env, variable) {
  const configured = String(env?.[variable] ?? "").trim();
  if (!configured) return undefined;
  const normalized = configured.toLowerCase().replace(/^sha256:/, "");
  if (!/^[0-9a-f]{64}$/.test(normalized)) {
    throw new Error(`${variable} must be 64 hex digits, optionally prefixed with sha256:`);
  }
  return normalized;
}

/**
 * The SHA-256 the deploy pinned for the manifest, from `MANIFEST_DIGEST`.
 *
 * Unset returns `undefined`: the per-part FNV hashes still run, they are just not
 * authentication. A malformed value throws, so a typo in the deploy fails the query
 * closed (503) instead of silently running unpinned against a substituted manifest.
 */
export function manifestDigest(env) {
  return pinnedDigest(env, "MANIFEST_DIGEST");
}

/**
 * The SHA-256 the deploy pinned for the curation file, from `CURATE_DIGEST`.
 *
 * Curation reorders hits, so a substituted rule file is a ranking change even when
 * the artifact is intact. The hash is checked before the file is decoded; a mismatch
 * leaves the ranking uncurated rather than failing the query, because rules are
 * optional policy and not part of the index.
 */
export function curationDigest(env) {
  return pinnedDigest(env, "CURATE_DIGEST");
}

/**
 * Resolve `{digest}` in an object key, so a set can live under a versioned prefix.
 *
 * `INDEX_KEY = "index/{digest}/manifest.txt"` reads the set the deploy built from
 * `index/<manifest digest>/`, which is what makes immutable caching safe: a rebuild
 * writes new keys instead of overwriting the ones a cached manifest still names.
 * A template that needs a digest the deploy did not set throws, so the Worker cannot
 * read a literal `{digest}` key and serve nothing.
 */
export function resolveKey(template, digest, variable = "INDEX_KEY", digestVariable = "MANIFEST_DIGEST") {
  const key = String(template ?? "").trim();
  if (!key.includes("{digest}")) return key;
  if (!digest) {
    throw new Error(`${variable} uses {digest}, so ${digestVariable} must be set`);
  }
  return key.split("{digest}").join(digest);
}

/** The artifact ceiling for this deployment, from `MAX_ARTIFACT_BYTES`. */
export function artifactLimit(env) {
  const configured = Number(env?.MAX_ARTIFACT_BYTES);
  if (!Number.isFinite(configured) || configured <= 0) return DEFAULT_MAX_ARTIFACT_BYTES;
  return Math.floor(configured);
}

/** The origins `ALLOWED_ORIGINS` names, trimmed; `[]` means same-origin only. */
export function allowedOrigins(env) {
  const raw = String(env?.ALLOWED_ORIGINS ?? "").trim();
  if (!raw) return [];
  return raw
    .split(",")
    .map((origin) => origin.trim())
    .filter((origin) => origin.length > 0);
}

/**
 * The CORS headers for one request.
 *
 * The origin is echoed only when the allowlist names it, so a response is never
 * readable by an origin the deployment did not list. `*` works only when the
 * deployment writes it: it is an explicit choice, not the default.
 */
export function corsHeaders(env, request) {
  const origin = request?.headers?.get?.("origin") || "";
  const allowed = allowedOrigins(env);
  if (!origin) return { vary: "origin" };
  if (allowed.includes("*")) {
    return { "access-control-allow-origin": "*", vary: "origin" };
  }
  if (!allowed.includes(origin)) return { vary: "origin" };
  return { "access-control-allow-origin": origin, vary: "origin" };
}

/** The preflight answer for an origin the allowlist names. */
export function preflightHeaders(env, request) {
  const origin = request?.headers?.get?.("origin") || "";
  const allowed = allowedOrigins(env);
  const listed = allowed.includes("*") || (origin !== "" && allowed.includes(origin));
  if (!listed) return { allow: ALLOWED_METHODS };
  return {
    allow: ALLOWED_METHODS,
    "access-control-allow-methods": ALLOWED_METHODS,
    "access-control-allow-headers": "accept, content-type",
    "access-control-max-age": "86400",
  };
}
