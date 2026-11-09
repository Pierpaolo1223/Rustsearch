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
 * The SHA-256 the deploy pinned for the manifest, from `MANIFEST_DIGEST`.
 *
 * `rustsearch shard` writes it to `manifest.txt.sha256` as `sha256:<hex>`, so the
 * prefix is accepted and stripped. Unset returns `undefined`: the per-part FNV
 * hashes still run, they are just not authentication. A malformed value throws,
 * so a typo in the deploy fails the query closed (503) instead of silently
 * running unpinned against a substituted manifest.
 */
export function manifestDigest(env) {
  const configured = String(env?.MANIFEST_DIGEST ?? "").trim();
  if (!configured) return undefined;
  const normalized = configured.toLowerCase().replace(/^sha256:/, "");
  if (!/^[0-9a-f]{64}$/.test(normalized)) {
    throw new Error("MANIFEST_DIGEST must be 64 hex digits, optionally prefixed with sha256:");
  }
  return normalized;
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
