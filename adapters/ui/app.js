// The page: one search box over one artifact, plus the query knobs the engine
// exposes. All engine access goes through ./core/search.js, and every node that
// carries artifact text is created with createElement/textContent — this file
// never assigns innerHTML.

import { openIndex, openSharded } from "./core/search.js";
import {
  formatBytes,
  formatCount,
  highlightSegments,
  parseFilterParam,
  pluralize,
  safeHref,
} from "./core/format.js";

const $ = (id) => document.getElementById(id);

const els = {
  form: $("search-form"),
  input: $("q"),
  clear: $("clear"),
  facets: $("facets"),
  summary: $("summary"),
  timing: $("timing"),
  state: $("state"),
  results: $("results"),
  more: $("more"),
  stats: $("engine-stats"),
  fuzzy: $("fuzzy"),
  group: $("group"),
  boost: $("boost"),
  boostValue: $("boost-value"),
  optionsBadge: $("options-badge"),
  theme: $("theme"),
  themeLabel: $("theme-label"),
  loadIndex: $("load-index"),
  indexFile: $("index-file"),
};

const DEFAULT_LIMIT = 10;
const MAX_LIMIT = 100;
const PAGE = 10;
const DEFAULT_BOOST = 2;
// The query has no debounce: a search is ~1 ms of WASM on the ten-thousand-document
// demo (0.096-0.119 ms p50 on the 2,000-document benchmark, 1.30 ms p50 with ten
// snippets on this corpus), so a 100 ms timer was the latency, not the protection.
// What does get a timer is the address bar: `history.replaceState` is rate-limited
// by the browser (WebKit allows ~100 calls per 30 s), and a keystroke is not a
// navigation event.
const URL_SYNC_MS = 250;
const THEMES = ["system", "light", "dark"];
const DEFAULT_SOURCE = "demo/index.rsx";
// The demo loads the v5 shard set by default: the generated corpus is ~11 MB and a
// query needs a few hundred KB of it. `?index=` (or Load index…) still opens the
// whole artifact, which is what the format-v4 docs used to do.
const DEFAULT_MANIFEST = "demo/shards/manifest.txt";

const state = {
  session: null,
  facets: {},
  query: "",
  limit: DEFAULT_LIMIT,
  fuzzy: false,
  group: false,
  boost: DEFAULT_BOOST,
  filters: new Map(),
  hits: [],
  links: [],
  expanded: new Set(),
  sourceParam: null,
  manifest: null,
  lastSource: DEFAULT_SOURCE,
  searchSeq: 0,
  loadSeq: 0,
  renderSeq: 0,
};

let urlTimer = 0;

const reduceMotion = () =>
  typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;

const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

/** Small createElement helper: text goes in as a text node, never as markup. */
function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else {
      node.setAttribute(key, value === true ? "" : value);
    }
  }
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    node.append(child);
  }
  return node;
}

function separator() {
  return el("span", { class: "sep", text: "·" });
}

/* ------------------------------------------------------------------ theme */

let preference = "system";
try {
  preference = localStorage.getItem("rustsearch-theme") || "system";
} catch {
  // Storage can be unavailable; the page still works with the system theme.
}
if (!THEMES.includes(preference)) preference = "system";
const darkQuery = typeof matchMedia === "function" ? matchMedia("(prefers-color-scheme: dark)") : null;

function applyTheme() {
  const resolved = preference === "system" ? (darkQuery?.matches ? "dark" : "light") : preference;
  document.documentElement.dataset.theme = resolved;
  els.theme.setAttribute("aria-label", `Color theme: ${preference}`);
  els.theme.title = `Color theme: ${preference} (click to change)`;
  els.themeLabel.textContent = preference === "system" ? "Auto" : preference === "light" ? "Light" : "Dark";
}

function cycleTheme() {
  preference = THEMES[(THEMES.indexOf(preference) + 1) % THEMES.length];
  try {
    localStorage.setItem("rustsearch-theme", preference);
  } catch {
    // Ignore: the preference just will not survive a reload.
  }
  applyTheme();
}

/* ------------------------------------------------------------------ state */

function syncControls() {
  els.input.value = state.query;
  els.clear.hidden = state.query.length === 0;
  els.fuzzy.checked = state.fuzzy;
  els.group.checked = state.group;
  els.boost.value = String(state.boost);
  els.boostValue.textContent = String(state.boost);

  const active = (state.fuzzy ? 1 : 0) + (state.group ? 1 : 0) + (state.boost !== DEFAULT_BOOST ? 1 : 0);
  els.optionsBadge.hidden = active === 0;
  els.optionsBadge.textContent = String(active);
}

function syncUrl() {
  const params = new URLSearchParams();
  const query = state.query.trim();
  if (query) params.set("q", query);
  if (state.limit !== DEFAULT_LIMIT) params.set("limit", String(state.limit));
  if (state.fuzzy) params.set("fuzzy", "1");
  if (state.group) params.set("group", "1");
  if (state.boost !== DEFAULT_BOOST) params.set("boost", String(state.boost));
  for (const [field, values] of state.filters) {
    for (const value of values) params.append("f", `${field}=${value}`);
  }
  if (state.manifest) params.set("shards", state.manifest);
  else if (state.sourceParam) params.set("index", state.sourceParam);
  // Written back, not just read: the first sync happens a moment after load, and
  // rebuilding the query string without it would drop the curation from the address
  // bar a reader had just opened.
  if (state.curation) params.set("curate", state.curation);
  const search = params.toString();
  try {
    history.replaceState(null, "", `${location.pathname}${search ? `?${search}` : ""}${location.hash}`);
  } catch {
    // A throttled replaceState must not take the search down with it.
  }
}

/** Write the address bar once the typing pauses; the query itself never waits. */
function scheduleUrlSync() {
  clearTimeout(urlTimer);
  urlTimer = setTimeout(() => {
    urlTimer = 0;
    syncUrl();
  }, URL_SYNC_MS);
}

function flushUrlSync() {
  if (!urlTimer) return;
  clearTimeout(urlTimer);
  urlTimer = 0;
  syncUrl();
}

function toggleFilter(field, value) {
  let values = state.filters.get(field);
  if (!values) {
    values = new Set();
    state.filters.set(field, values);
  }
  if (values.has(value)) values.delete(value);
  else values.add(value);
  if (values.size === 0) state.filters.delete(field);
  state.limit = DEFAULT_LIMIT;
  renderFacets();
  runSearch();
}

function clearFilters() {
  state.filters.clear();
  state.limit = DEFAULT_LIMIT;
  renderFacets();
  runSearch();
}

/* ---------------------------------------------------------------- facets */

function renderFacets() {
  const entries = Object.entries(state.facets);
  if (entries.length === 0) {
    // Keep the skeletons shipped in index.html until an index reports its facets:
    // removing the bar before the first paint is a layout shift of its own.
    if (!state.session) return;
    els.facets.hidden = true;
    els.facets.replaceChildren();
    return;
  }

  els.facets.hidden = false;
  const bar = el("div", { class: "facet-bar" });

  entries.sort(([a], [b]) => a.localeCompare(b));
  for (const [field, values] of entries) {
    const sorted = Object.entries(values).sort(
      ([nameA, countA], [nameB, countB]) => countB - countA || nameA.localeCompare(nameB),
    );
    const group = el("div", { class: "facet", role: "group", "aria-label": `${field} filters` });
    group.append(el("span", { class: "facet-label", text: field }));

    const expanded = state.expanded.has(field);
    const visible = expanded ? sorted : sorted.slice(0, 6);
    for (const [value, count] of visible) {
      const pressed = state.filters.get(field)?.has(value) ?? false;
      const chip = el(
        "button",
        {
          class: "chip",
          type: "button",
          "aria-pressed": pressed ? "true" : "false",
          title: `${formatCount(count)} documents in ${field}=${value}`,
        },
        el("span", { text: value }),
        el("span", { class: "count", text: formatCount(count) }),
      );
      chip.addEventListener("click", () => toggleFilter(field, value));
      group.append(chip);
    }

    if (sorted.length > 6) {
      const more = el(
        "button",
        {
          class: "chip more",
          type: "button",
          "aria-expanded": expanded ? "true" : "false",
        },
        expanded ? "Less" : `+${sorted.length - 6} more`,
      );
      more.addEventListener("click", () => {
        if (expanded) state.expanded.delete(field);
        else state.expanded.add(field);
        renderFacets();
      });
      group.append(more);
    }

    bar.append(group);
  }

  if (state.filters.size > 0) {
    const count = [...state.filters.values()].reduce((total, values) => total + values.size, 0);
    const clear = el("button", {
      class: "clear-filters",
      type: "button",
      text: `Clear ${pluralize(count, "filter")}`,
    });
    clear.addEventListener("click", clearFilters);
    bar.append(clear);
  }

  els.facets.replaceChildren(bar);
}

/* --------------------------------------------------------------- results */

function renderStats(stats) {
  const line = [
    el("span", { text: `${formatCount(stats.docCount)} documents` }),
    separator(),
    el("span", { text: `${formatCount(stats.termCount)} terms` }),
    separator(),
    el("span", { text: `analyzer ${stats.analyzer}` }),
    separator(),
    el("span", { text: formatBytes(stats.byteLength) }),
    separator(),
    el("span", { text: `format v${stats.formatVersion}` }),
    separator(),
    el("span", { text: `checksum ${stats.checksum}` }),
    separator(),
    el("span", { text: stats.label }),
  ];
  if (stats.sharded && stats.transfer) {
    line.push(
      separator(),
      el("span", {
        text: `sharded · ${formatCount(stats.transfer.requests)} requests · ${formatBytes(stats.transfer.bytes)} read`,
      }),
    );
  }
  // Whether `?curate=` is in force is not visible anywhere else, and a rejected file
  // has to say so: otherwise a curated ranking and a plain one look identical.
  if (state.curationNote) line.push(separator(), el("span", { text: state.curationNote }));
  els.stats.replaceChildren(...line);
}

function appendSegments(parent, text, ranges) {
  for (const segment of highlightSegments(text, ranges)) {
    if (segment.mark) parent.append(el("mark", { text: segment.text }));
    else parent.append(document.createTextNode(segment.text));
  }
}

function renderHit(hit) {
  const item = el("li", { class: "hit" });
  const head = el("div", { class: "hit-head" });
  const title = el("h3", { class: "hit-title" });
  const href = safeHref(hit.url);

  if (href) {
    const link = el("a", { href });
    link.textContent = hit.title || hit.url;
    title.append(link);
  } else {
    const plain = el("span", { class: "plain" });
    plain.textContent = hit.title || hit.url || "Untitled";
    title.append(plain);
  }

  const score = Number(hit.score);
  head.append(
    title,
    el("span", {
      class: "score",
      title: "BM25F score",
      text: Number.isFinite(score) ? score.toFixed(4) : "—",
    }),
  );
  item.append(head);

  if (hit.url) item.append(el("p", { class: "url", text: hit.url }));
  if (hit.snippet) {
    const snippet = el("p", { class: "snippet" });
    appendSegments(snippet, hit.snippet, hit.highlights);
    item.append(snippet);
  }

  const meta = el("p", { class: "hit-meta" });
  meta.append(el("span", { text: `doc ${hit.docId}` }));
  if (hit.matches > 1) meta.append(el("span", { text: `${formatCount(hit.matches)} chunks` }));
  if (state.group) meta.append(el("span", { text: "grouped by page" }));
  item.append(meta);

  return item;
}

// A hundred-hit frame is ~7 ms of DOM work, which is a long task on a keystroke's
// deadline. Rows are appended in small animation-frame slices instead: the first
// screenful is painted immediately, each slice costs ~1 ms, and `content-visibility`
// in the stylesheet lets the browser skip layout and paint below the fold.
const RENDER_SLICE = 12;

function renderResults(payload, query) {
  state.hits = payload.results;
  state.links = [];
  const token = ++state.renderSeq;
  els.results.replaceChildren();

  let next = 0;
  const paint = () => {
    if (token !== state.renderSeq) return;
    const fragment = document.createDocumentFragment();
    const items = [];
    const end = Math.min(next + RENDER_SLICE, state.hits.length);
    for (; next < end; next += 1) {
      const item = renderHit(state.hits[next]);
      items.push(item);
      fragment.append(item);
    }
    els.results.append(fragment);
    // Appended in document order, so the keyboard order stays the reading order.
    for (const item of items) {
      const link = item.querySelector(".hit-title a");
      if (link) state.links.push(link);
    }
    if (next < state.hits.length) requestAnimationFrame(paint);
  };
  paint();

  const count = state.hits.length;
  const atLimit = count >= state.limit;
  els.summary.textContent = atLimit
    ? `First ${formatCount(count)} results for “${query}”`
    : `${pluralize(count, "result")} for “${query}”`;
  els.timing.textContent = `${payload.elapsed.toFixed(1)} ms · WASM query + JSON`;

  els.more.hidden = !(atLimit && state.limit < MAX_LIMIT);
  els.more.textContent = `Show ${Math.min(PAGE, MAX_LIMIT - state.limit)} more`;
}

function showResults() {
  els.state.replaceChildren();
  els.results.hidden = false;
}

function showIdle() {
  state.hits = [];
  state.links = [];
  state.renderSeq += 1;  // stop a render slice that is still appending rows
  els.results.replaceChildren();
  els.results.hidden = false;
  els.more.hidden = true;
  els.summary.textContent = state.session
    ? `Search ${formatCount(state.session.stats.docCount)} documents`
    : "Search the index";
  els.timing.textContent = "";

  const docs = state.session ? formatCount(state.session.stats.docCount) : "the";
  const card = el(
    "div",
    { class: "card" },
    el("h3", { text: `Search ${docs} documents` }),
    el("p", {
      text:
        "The index was built at deploy time and is read here through WebAssembly. " +
        "Facets filter before ranking; typo tolerance, title boost and page grouping " +
        "are query-time options.",
    }),
  );
  const suggestions = el("div", { class: "suggestions" });
  for (const query of ["postgres", "security", "immutable index", "docker compose"]) {
    suggestions.append(
      el("button", { class: "hint-query", type: "button", "data-query": query, text: query }),
    );
  }
  card.append(suggestions);
  els.state.replaceChildren(card);
}

function showEmpty(query) {
  els.state.replaceChildren();
  state.renderSeq += 1;
  els.results.replaceChildren();
  state.links = [];
  els.more.hidden = true;
  els.summary.textContent = `No results for “${query}”`;
  els.timing.textContent = "";

  const card = el(
    "div",
    { class: "card" },
    el("h3", { text: `Nothing matched “${query}”` }),
    el("p", {
      text:
        "The analyzer drops stopwords, so a query made only of them returns nothing. " +
        "Every word has to appear in the index (stemmed, so “indexing” finds “index”).",
    }),
  );

  const actions = el("div", { class: "card-actions" });
  if (!state.fuzzy) {
    const retry = el("button", { class: "button", type: "button", text: "Retry with typo tolerance" });
    retry.addEventListener("click", () => {
      state.fuzzy = true;
      syncControls();
      runSearch();
    });
    actions.append(retry);
  }
  const filterCount = [...state.filters.values()].reduce((total, values) => total + values.size, 0);
  if (filterCount > 0) {
    const clear = el("button", {
      class: "button",
      type: "button",
      text: `Remove ${pluralize(filterCount, "filter")}`,
    });
    clear.addEventListener("click", clearFilters);
    actions.append(clear);
  }
  if (actions.childElementCount > 0) card.append(actions);
  els.state.replaceChildren(card);
}

function showLoading() {
  state.renderSeq += 1;
  els.results.replaceChildren();
  state.links = [];
  els.more.hidden = true;
  els.summary.textContent = "Loading the index…";
  els.timing.textContent = "";

  const skeleton = el("div", { class: "skeleton", "aria-hidden": "true" });
  for (let index = 0; index < 3; index += 1) skeleton.append(el("div", { class: "skeleton-row" }));
  els.state.replaceChildren(skeleton);
}

function showError(error, context) {
  const isLoad = context === "load";
  state.renderSeq += 1;
  els.results.replaceChildren();
  state.links = [];
  els.more.hidden = true;
  // There is no index to describe, so the facet placeholder must not keep shimmering.
  if (isLoad) els.facets.hidden = true;
  els.summary.textContent = isLoad ? "The index could not be loaded" : "The query could not run";
  els.timing.textContent = "";

  const card = el(
    "div",
    { class: "card error-card", role: "alert" },
    el("h3", { text: isLoad ? "Could not load the index" : "Could not run the query" }),
    el("p", { text: error.message ?? String(error) }),
  );

  if (isLoad) {
    card.append(
      el("p", {
        text:
          "The default demo is built by ./setup.sh into demo/index.rsx and demo/shards/. " +
          "If they are there, serve the page over HTTP: fetch() does not read a file:// URL.",
      }),
    );
  }

  const actions = el("div", { class: "card-actions" });
  const retry = el("button", { class: "button", type: "button", text: "Retry" });
  retry.addEventListener("click", () => {
    if (isLoad) {
      // Retry the source that failed: the default is a shard set, not the artifact.
      if (state.manifest) loadShards(state.manifest);
      else loadIndex(state.lastSource);
    } else runSearch();
  });
  actions.append(retry);

  if (isLoad) {
    const choose = el("button", { class: "button", type: "button", text: "Load another index…" });
    choose.addEventListener("click", () => els.indexFile.click());
    actions.append(choose);
  }

  card.append(actions);
  els.state.replaceChildren(card);
}

/* ---------------------------------------------------------------- search */

async function runSearch() {
  if (!state.session) return;
  const session = state.session;
  const query = state.query.trim();
  const sequence = ++state.searchSeq;

  if (!query) {
    showIdle();
    scheduleUrlSync();
    return;
  }

  let payload;
  try {
    payload = await session.search({
      query,
      limit: state.limit,
      fuzzy: state.fuzzy,
      maxEdits: 1,
      titleBoost: state.boost,
      group: state.group,
      filters: state.filters,
    });
  } catch (error) {
    if (sequence === state.searchSeq) showError(error, "query");
    return;
  }
  // `null` is a query the session dropped because a newer one was waiting; the
  // sequence check covers a result that is stale anyway (query changed, index
  // replaced) and must not paint over the current one.
  if (!payload || sequence !== state.searchSeq || session !== state.session) return;

  // A sharded index reports what the query actually read; the footer follows it
  // instead of showing only the metadata cost of the last load.
  if (payload.transfer) renderStats({ ...session.stats, transfer: payload.transfer });

  if (payload.results.length === 0) {
    state.hits = [];
    showEmpty(query);
  } else {
    renderResults(payload, query);
    showResults();
  }
  scheduleUrlSync();
}

async function loadSession(open, label) {
  const sequence = ++state.loadSeq;
  showLoading();
  const previous = state.session;

  try {
    const session = await open();
    if (sequence !== state.loadSeq) {
      session.dispose();
      return;
    }
    if (previous) previous.dispose();
    state.session = session;
    // Awaited: `?q=` runs a query as soon as the index is ready, and a curation file
    // fetched in parallel could land after it — the first answer would be plain BM25F
    // while every later one is curated.
    if (state.curation) await applyCurations(session);
    state.facets = session.facets();
    renderStats(session.stats);
    renderFacets();
    if (state.query.trim()) runSearch();
    else {
      showIdle();
      scheduleUrlSync();
    }
    syncControls();
  } catch (error) {
    if (sequence !== state.loadSeq) return;
    state.session = previous;
    if (!previous) els.stats.textContent = "No index loaded.";    showError(error, "load");
  }
}

function loadIndex(source, label) {
  state.lastSource = source;
  return loadSession(() => openIndex(source, label ?? source), label ?? source);
}

/** `?shards=demo/shards/manifest.txt`: metadata first, texts per hit. */
function loadShards(manifest) {
  return loadSession(() => openSharded(manifest), "sharded index");
}

/**
 * `?curate=curated/curate.txt`: fetch the editor's rules and apply them.
 *
 * The file is optional: a 404, a network error or a malformed rule leaves the
 * ranking exactly as the engine produced it, because curation is a convenience and
 * not part of the index. The engine parses the text with the same implementation the
 * CLI uses, so `pin` and `boost` behave identically here and in `search --curate`.
 */
async function applyCurations(session) {
  try {
    const response = await fetch(new URL(state.curation, document.baseURI), { cache: "no-cache" });
    // A missing file is a deployment with no curation, not an error.
    if (!response.ok) return;
    const text = (await response.text()).slice(0, 64 * 1024);
    if (state.session !== session) return;
    const count = await session.setCurations(text);
    if (count === null || state.session !== session) return;
    state.curationNote = `${formatCount(count)} curated ${count === 1 ? "query" : "queries"}`;
    renderStats(session.stats);
  } catch (error) {
    if (state.session !== session) return;
    // The engine refused the file: keep the plain BM25F ranking and say so, instead of
    // showing a ranking the reader believes is curated.
    state.curationNote = `curation rejected: ${error.message ?? error}`;
    renderStats(session.stats);
  }
}

function selectQuery(query) {
  state.query = query;
  state.limit = DEFAULT_LIMIT;
  syncControls();
  runSearch();
  els.input.focus();
}

/* -------------------------------------------------------------- keyboard */

function focusHit(index) {
  const link = state.links[index];
  if (!link) return;
  link.focus({ preventScroll: true });
  link.scrollIntoView({ block: "nearest", behavior: reduceMotion() ? "auto" : "smooth" });
}

function handleResultsKeydown(event) {
  const index = state.links.indexOf(document.activeElement);
  if (index < 0) return;
  if (event.key === "ArrowDown") {
    event.preventDefault();
    focusHit(Math.min(state.links.length - 1, index + 1));
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    if (index === 0) els.input.focus();
    else focusHit(index - 1);
  } else if (event.key === "Home") {
    event.preventDefault();
    focusHit(0);
  } else if (event.key === "End") {
    event.preventDefault();
    focusHit(state.links.length - 1);
  } else if (event.key === "Escape") {
    event.preventDefault();
    els.input.focus();
  }
}

/* ------------------------------------------------------------------- init */

function readUrl() {
  const params = new URLSearchParams(location.search);
  state.query = params.get("q") ?? "";
  const limit = Number(params.get("limit"));
  state.limit = Number.isFinite(limit) && limit >= 1 ? clamp(Math.trunc(limit), 1, MAX_LIMIT) : DEFAULT_LIMIT;
  // `Number(null)` is 0 and 0 is a legal boost, so an absent parameter has to be
  // told apart from `?boost=0` before converting: the default is a boost of 2.
  const boostParam = params.get("boost")?.trim();
  const boost = boostParam ? Number(boostParam) : Number.NaN;
  state.boost = Number.isFinite(boost) && boost >= 0 ? clamp(Math.trunc(boost), 0, 10) : DEFAULT_BOOST;
  state.fuzzy = params.get("fuzzy") === "1";
  state.group = params.get("group") === "1";
  state.filters = new Map();
  for (const raw of params.getAll("f")) {
    const parsed = parseFilterParam(raw);
    if (!parsed) continue;
    const [field, value] = parsed;
    if (!state.filters.has(field)) state.filters.set(field, new Set());
    state.filters.get(field).add(value);
  }
  const index = params.get("index");
  state.sourceParam = index && safeHref(index) ? index : null;
  const shards = params.get("shards");
  state.manifest = shards && safeHref(shards) ? shards : null;
  // `?curate=curated/curate.txt`: the editor's pin/boost file, fetched once and
  // handed to the engine. Absent means the plain ranking.
  const curate = params.get("curate");
  state.curation = curate && safeHref(curate) ? curate : null;
}

function init() {
  applyTheme();
  readUrl();
  syncControls();
  renderFacets();

  if (state.manifest || !state.sourceParam) {
    state.manifest = state.manifest || DEFAULT_MANIFEST;
    loadShards(state.manifest);
  } else {
    loadIndex(state.sourceParam, state.sourceParam);
  }

  els.form.addEventListener("submit", (event) => {
    event.preventDefault();
    state.limit = DEFAULT_LIMIT;
    runSearch();
  });

  // Every keystroke is a query. `runSearch` keeps the sequence guard for the
  // async paths (index load, a future Worker); an input event that is part of an
  // IME composition is skipped so a half-composed string never reaches the engine.
  els.input.addEventListener("input", (event) => {
    state.query = els.input.value;
    els.clear.hidden = state.query.length === 0;
    if (event.isComposing) {
      scheduleUrlSync();
      return;
    }
    state.limit = DEFAULT_LIMIT;
    runSearch();
  });

  els.input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" && state.links.length > 0) {
      event.preventDefault();
      focusHit(0);
    } else if (event.key === "Escape") {
      event.preventDefault();
      if (state.query) {
        selectQuery("");
      } else {
        els.input.blur();
      }
    }
  });

  els.clear.addEventListener("click", () => {
    selectQuery("");
  });

  els.input.addEventListener("blur", flushUrlSync);
  window.addEventListener("pagehide", flushUrlSync);

  els.results.addEventListener("keydown", handleResultsKeydown);

  els.more.addEventListener("click", () => {
    state.limit = Math.min(MAX_LIMIT, state.limit + PAGE);
    runSearch();
  });

  for (const id of ["fuzzy", "group"]) {
    els[id].addEventListener("change", () => {
      state[id] = els[id].checked;
      state.limit = DEFAULT_LIMIT;
      syncControls();
      runSearch();
    });
  }

  els.boost.addEventListener("input", () => {
    els.boostValue.textContent = els.boost.value;
  });
  els.boost.addEventListener("change", () => {
    state.boost = Number(els.boost.value);
    state.limit = DEFAULT_LIMIT;
    syncControls();
    runSearch();
  });

  els.theme.addEventListener("click", cycleTheme);
  darkQuery?.addEventListener("change", () => {
    if (preference === "system") applyTheme();
  });

  els.loadIndex.addEventListener("click", () => els.indexFile.click());
  els.indexFile.addEventListener("change", async () => {
    const file = els.indexFile.files?.[0];
    els.indexFile.value = "";
    if (!file) return;
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      state.sourceParam = null;
      state.manifest = null;
      loadIndex(bytes, file.name);
    } catch (error) {
      showError(error, "load");
    }
  });

  document.addEventListener("click", (event) => {
    if (!(event.target instanceof Element)) return;
    const query = event.target.closest(".hint-query[data-query]")?.dataset.query;
    if (query) selectQuery(query);
  });

  document.addEventListener("keydown", (event) => {
    const target = event.target;
    const typing =
      target instanceof HTMLElement &&
      (target.tagName === "INPUT" ||
        target.tagName === "TEXTAREA" ||
        target.tagName === "SELECT" ||
        target.isContentEditable);
    if (event.key === "/" && !typing && !event.metaKey && !event.ctrlKey && !event.altKey) {
      event.preventDefault();
      els.input.focus();
      els.input.select();
    }
  });

  window.addEventListener("popstate", () => {
    readUrl();
    syncControls();
    renderFacets();
    if (state.query.trim()) runSearch();
    else showIdle();
  });
}

init();
