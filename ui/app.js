/* llmproxy admin UI. No dependencies, no build step. */
"use strict";

const $ = (sel) => document.querySelector(sel);

/* Helpers */

function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, val] of Object.entries(attrs)) {
    if (key === "class") node.className = val;
    else if (key === "text") node.textContent = val;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), val);
    else if (val !== null && val !== undefined) node.setAttribute(key, val);
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined) continue;
    node.append(child.nodeType ? child : document.createTextNode(child));
  }
  return node;
}

async function apiGet(path) {
  const response = await fetch(path, { headers: { Accept: "application/json" } });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body?.error?.message || `${path}: HTTP ${response.status}`);
  return body;
}

async function apiSend(path, method, body) {
  const response = await fetch(path, {
    method,
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const parsed = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(parsed?.error?.message || `${path}: HTTP ${response.status}`);
  return parsed;
}

function toast(message, kind = "ok", detail = "") {
  const node = el("div", { class: `toast${kind === "error" ? " is-error" : kind === "warn" ? " is-warn" : ""}` }, [
    message,
    detail ? el("span", { class: "toast-detail", text: detail }) : null,
  ]);
  $("#toast-stack").append(node);
  setTimeout(() => node.remove(), kind === "error" ? 7000 : 4200);
}

function applyToast(result, what) {
  if (result.persisted) toast(`${what} applied and saved to the config file.`);
  else toast(`${what} applied.`, "warn", result.warning || "Not written to a config file.");
}

const fmtInt = new Intl.NumberFormat("en-US");

function fmtCount(n) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1) + "M";
  if (n >= 10_000) return (n / 1_000).toFixed(n >= 100_000 ? 0 : 1) + "k";
  return fmtInt.format(n);
}

function fmtMs(ms) {
  if (ms === null || ms === undefined) return "—";
  if (ms >= 10_000) return (ms / 1000).toFixed(1) + " s";
  if (ms >= 1000) return (ms / 1000).toFixed(2) + " s";
  return ms + " ms";
}

function fmtUptime(secs) {
  if (secs < 90) return `${secs}s up`;
  const m = Math.floor(secs / 60);
  if (m < 90) return `${m}m up`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m up`;
  return `${Math.floor(h / 24)}d ${h % 24}h up`;
}

/* Navigation */

const PRIMARY_VIEWS = ["overview", "activity", "routing", "models", "providers", "settings"];

function currentRoute() {
  const parts = location.hash.replace(/^#\/?/, "").split("/").filter(Boolean);
  let primary = parts[0] || "overview";
  let secondary = parts[1] || "";

  // Preserve bookmarks from the previous four-tab layout.
  if (primary === "monitor") primary = "overview";
  if (primary === "configure") {
    primary = "routing";
    secondary = "triage";
  }
  if (primary === "prompt") {
    primary = "routing";
    secondary = "policy";
  }
  if (!PRIMARY_VIEWS.includes(primary)) primary = "overview";
  if (primary === "activity" && !["usage", "logs"].includes(secondary)) secondary = "usage";
  if (primary === "routing" && !["triage", "policy"].includes(secondary)) secondary = "triage";
  return { primary, secondary };
}

function renderSubview(group, selected, names) {
  for (const name of names) {
    const active = selected === name;
    $(`#subview-${group}-${name}`).hidden = !active;
    $(`#subtab-${group}-${name}`).setAttribute("aria-selected", String(active));
  }
}

function renderTab() {
  const { primary, secondary } = currentRoute();
  for (const name of PRIMARY_VIEWS) {
    const active = primary === name;
    $(`#view-${name}`).hidden = !active;
    if (active) $(`#tab-${name}`).setAttribute("aria-current", "page");
    else $(`#tab-${name}`).removeAttribute("aria-current");
  }
  renderSubview("activity", primary === "activity" ? secondary : "usage", ["usage", "logs"]);
  renderSubview("routing", primary === "routing" ? secondary : "triage", ["triage", "policy"]);
  document.title = `llmproxy · ${primary}`;

  if (primary === "models") {
    loadModelsTab();
    loadConfigEditors();
  } else if (["routing", "providers", "settings"].includes(primary)) {
    loadConfigEditors();
  }
  if (primary === "routing" && secondary === "policy") loadPromptTab();
}

window.addEventListener("hashchange", () => {
  renderTab();
  window.scrollTo(0, 0);
});

/* Overview */

let launchData = null;

function launchSelection() {
  const agent = document.querySelector('input[name="agent"]:checked')?.value || "codex";
  const style = document.querySelector('input[name="style"]:checked')?.value || "launcher";
  return { agent, style };
}

function renderLaunch() {
  if (!launchData) return;
  const { agent, style } = launchSelection();
  const key = `${agent}_${style}`;
  $("#launch-command").textContent = launchData.commands[key] || "—";

  const hints = [];
  if (style === "proxy") {
    hints.push(
      `Routes every model request to ${launchData.default_provider}; only protocol and message structure are remapped.`
    );
    hints.push("No interceptor prompt, routing controls, or integrated provider tools are added.");
  } else if (style === "launcher") {
    const agentLabel = agent === "codex" ? "Codex" : agent === "claude" ? "Claude Code" : "Copilot CLI";
    hints.push(
      `Starts the proxy and ${agentLabel} together; ` +
      "the agent keeps its own login and the proxy daemon stays running."
    );
    hints.push(
      launchData.interceptor_configured
        ? `--intercept routes deterministic tasks to ${launchData.interceptor_model} for this session.`
        : "Configure the interceptor provider and model under Routing first — the launcher requires them."
    );
  } else if (style === "integrated") {
    hints.push(
      launchData.integrated_targets
        ? `Model Auto routes through ${launchData.integrated_targets} configured integrated target${launchData.integrated_targets === 1 ? "" : "s"}; explicit models bypass integrated triage.`
        : "Add integrated targets before launching; integrated mode will reject an empty target map."
    );
    hints.push("Cross-provider calls use only credentials owned by llmproxy.");
  } else if (agent === "codex") {
    hints.push("Add the profile to ~/.codex/config.toml, then start Codex with the last line.");
  } else if (agent === "claude") {
    hints.push("Points Claude Code at this already-running proxy via environment variables.");
  } else {
    hints.push("Native Copilot CAPI routing is request-scoped; use the launcher method.");
  }
  if (style === "env") {
    hints.push(
      launchData.interceptor_enabled
        ? "The interceptor is enabled, so eligible requests are answered locally."
        : "The interceptor is currently disabled; requests follow normal model remapping."
    );
  }
  $("#launch-hint").textContent = hints.join(" ");
}

async function copyLaunchCommand() {
  const text = $("#launch-command").textContent;
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      const area = el("textarea", { style: "position:fixed;opacity:0" }, [text]);
      document.body.append(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    const button = $("#copy-launch");
    button.textContent = "Copied";
    setTimeout(() => (button.textContent = "Copy"), 1300);
  } catch {
    toast("Copy failed — select the text manually.", "error");
  }
}

/* Provider name → local/external class, cached from /api/models so the
   per-provider stats table can reuse the locality colors. */
let providerClasses = {};

function renderRouting(data) {
  providerClasses = Object.fromEntries(
    data.providers.map((provider) => [provider.name, provider.class])
  );
  const list = $("#provider-list");
  list.replaceChildren();
  for (const provider of data.providers) {
    const models = provider.models_fetched
      ? `${provider.models.length} model${provider.models.length === 1 ? "" : "s"}`
      : "models not loaded";
    const title = provider.models.length ? provider.models.join("\n") : undefined;
    list.append(
      el("li", { class: "provider-item" }, [
        el("span", { class: "provider-name", text: provider.name }),
        el("span", { class: `badge badge-${provider.class}`, text: provider.class }),
        provider.is_default ? el("span", { class: "badge badge-neutral", text: "default" }) : null,
        provider.is_interceptor ? el("span", { class: "badge badge-neutral", text: "interceptor" }) : null,
        el("span", { class: "provider-url", text: provider.base_url }),
        el("span", { class: "provider-models", text: models, title }),
      ])
    );
  }

  const tbody = $("#alias-table tbody");
  tbody.replaceChildren();
  for (const alias of data.aliases) {
    tbody.append(
      el("tr", {}, [
        el("td", { text: alias.alias }),
        el("td", { class: "arrow-cell", text: "→" }),
        el("td", {}, [
          `${alias.model} `,
          el("span", { class: "badge badge-neutral", text: alias.provider }),
        ]),
      ])
    );
  }
  if (!data.aliases.length) {
    tbody.append(el("tr", {}, [el("td", { colspan: "3", text: "no aliases configured" })]));
  }

  $("#alias-hint").textContent = data.auto_mapping
    ? data.default_auto_model
      ? `Other model names map automatically to ${data.default_auto_model}.`
      : "Other model names map automatically to the default provider's first model."
    : "Automatic remapping is off (passthrough session); original model names are preserved.";

  const roles = Object.entries(data.role_map).sort();
  $("#rolemap-line").textContent = roles.length
    ? roles.map(([from, to]) => `${from} → ${to}`).join("  ·  ")
    : "none";
}

function setLatencyRow(barId, figuresId, entry, maxP99) {
  const bar = $(barId);
  const figures = $(figuresId);
  if (!entry.samples) {
    bar.classList.add("hidden");
    figures.textContent = "no samples";
    return;
  }
  bar.classList.remove("hidden");
  // sqrt scale keeps fast local bars visible next to slow external ones.
  const pos = (v) => Math.sqrt(Math.min(v, maxP99) / maxP99) * 100;
  const left = pos(entry.p50_ms);
  const right = Math.max(pos(entry.p99_ms), left + 1);
  bar.style.left = left.toFixed(2) + "%";
  bar.style.width = Math.min(right - left, 100 - left).toFixed(2) + "%";
  figures.textContent = `${fmtMs(entry.p50_ms)} → ${fmtMs(entry.p99_ms)} · ${entry.samples}`;
}

function renderStats(stats) {
  $("#meta-uptime").textContent = fmtUptime(stats.uptime_secs);

  const requests = stats.requests;
  const served = stats.served;
  const intercept = stats.intercept;
  const tokens = stats.tokens_saved;

  $("#stat-requests").textContent = fmtCount(requests.total);
  $("#stat-remapped").textContent = fmtCount(requests.remapped);
  $("#stat-remapped-sub").textContent =
    `alias ${fmtCount(requests.remap_alias)} · namespace ${fmtCount(requests.remap_namespace || 0)} · auto ${fmtCount(requests.remap_auto)}`;
  $("#stat-intercepted").textContent = fmtCount(intercept.attempts);
  $("#stat-intercepted-sub").textContent =
    `fallthrough ${fmtCount(intercept.pass)} · redirect ${fmtCount(intercept.redirect || 0)} · fallback ${fmtCount(intercept.fallback)}` +
    (intercept.bypass ? ` · bypass ${fmtCount(intercept.bypass)}` : "") +
    (intercept.errors ? ` · errors ${fmtCount(intercept.errors)}` : "");
  $("#stat-local").textContent = fmtCount(served.local);
  $("#stat-local-sub").textContent = intercept.local ? `${fmtCount(intercept.local)} by interceptor` : " ";
  $("#stat-external").textContent = fmtCount(served.external);
  $("#stat-tokens").textContent = "≈ " + fmtCount(tokens.total);
  $("#stat-tokens-sub").textContent = `prompt ${fmtCount(tokens.prompt)} · completion ${fmtCount(tokens.completion)}`;
  renderRoutingMatrix(served.matrix || {});
  renderProviderCalls(served.providers || {});
  renderRoutingDestinations(served.destinations || []);
  renderModelUsage(stats.model_usage || []);

  // Flow strip: stroke weight follows each branch's share of responses.
  const totalServed = served.local + served.external;
  $("#flow-in-count").textContent = fmtCount(requests.total);
  $("#flow-local-count").textContent = `${fmtCount(served.local)} local`;
  $("#flow-ext-count").textContent = `${fmtCount(served.external)} external`;
  const width = (n) => (totalServed ? 2 + 5 * (n / totalServed) : 2).toFixed(2);
  const flowLocal = $("#flow-local");
  const flowExt = $("#flow-ext");
  flowLocal.style.strokeWidth = width(served.local);
  flowExt.style.strokeWidth = width(served.external);
  flowLocal.classList.toggle("idle", !served.local);
  flowExt.classList.toggle("idle", !served.external);
  $("#flow-in").classList.toggle("idle", !requests.total);
  $("#flow-in").style.strokeWidth = requests.total ? "2.5" : "2";

  const band = $("#split-band");
  if (totalServed) {
    band.classList.remove("is-empty");
    const share = (served.local / totalServed) * 100;
    $("#split-local").style.width = share.toFixed(1) + "%";
    $("#split-caption").textContent =
      `${share.toFixed(share >= 10 ? 0 : 1)}% of ${fmtCount(totalServed)} responses stayed local`;
  } else {
    band.classList.add("is-empty");
    $("#split-local").style.width = "0%";
    $("#split-caption").textContent = "no responses yet";
  }

  const maxP99 = Math.max(stats.latency.local.p99_ms || 0, stats.latency.external.p99_ms || 0, 1);
  setLatencyRow("#lat-local-bar", "#lat-local-figures", stats.latency.local, maxP99);
  setLatencyRow("#lat-ext-bar", "#lat-ext-figures", stats.latency.external, maxP99);
}

function renderRoutingMatrix(matrix) {
  const table = $("#routing-matrix");
  const head = table.querySelector("thead");
  const body = table.querySelector("tbody");
  const frontends = Object.keys(matrix).sort();
  const providers = [...new Set(frontends.flatMap((frontend) => Object.keys(matrix[frontend] || {})))].sort();
  head.replaceChildren(
    el("tr", {}, [
      el("th", { text: "frontend" }),
      ...providers.map((provider) => el("th", { class: "mono", text: provider })),
    ])
  );
  body.replaceChildren();
  if (!frontends.length) {
    body.append(el("tr", {}, [el("td", { colspan: String(Math.max(2, providers.length + 1)), text: "no completed routes yet" })]));
    return;
  }
  for (const frontend of frontends) {
    body.append(
      el("tr", {}, [
        el("td", { class: "mono", text: frontend }),
        ...providers.map((provider) => el("td", { class: "mono", text: fmtCount(matrix[frontend]?.[provider] || 0) })),
      ])
    );
  }
}

function renderProviderCalls(providers) {
  const body = $("#provider-calls tbody");
  body.replaceChildren();
  const names = Object.keys(providers).sort();
  if (!names.length) {
    body.append(el("tr", {}, [el("td", { colspan: "5", text: "no completed calls yet" })]));
    return;
  }
  const total = names.reduce((sum, name) => sum + (providers[name].count || 0), 0);
  for (const name of names) {
    const entry = providers[name];
    const cls = providerClasses[name];
    const share = total ? (entry.count / total) * 100 : 0;
    body.append(
      el("tr", {}, [
        el("td", { class: "mono" }, [
          `${name} `,
          cls ? el("span", { class: `badge badge-${cls}`, text: cls }) : null,
        ]),
        el("td", { class: "mono", text: fmtCount(entry.count) }),
        el("td", { class: "mono", text: share.toFixed(share >= 10 ? 0 : 1) + "%" }),
        el("td", { class: "mono", text: entry.samples ? fmtMs(entry.p50_ms) : "—" }),
        el("td", { class: "mono", text: entry.samples ? fmtMs(entry.p99_ms) : "—" }),
      ])
    );
  }
}

function renderRoutingDestinations(destinations) {
  const body = $("#routing-destinations tbody");
  body.replaceChildren();
  if (!destinations.length) {
    body.append(el("tr", {}, [el("td", { colspan: "5", text: "no routed requests yet" })]));
    return;
  }
  for (const entry of destinations) {
    body.append(
      el("tr", {}, [
        el("td", { class: "mono", text: entry.frontend }),
        el("td", { class: "mono", text: entry.provider }),
        el("td", { class: "mono", text: entry.model }),
        el("td", { class: "mono", text: fmtCount(entry.requests) }),
        el("td", { class: entry.errors ? "mono status-bad" : "mono", text: fmtCount(entry.errors) }),
      ])
    );
  }
}

function renderModelUsage(models) {
  const body = $("#model-usage tbody");
  body.replaceChildren();
  if (!models.length) {
    body.append(el("tr", {}, [el("td", { colspan: "6", text: "no completed model calls yet" })]));
    return;
  }
  for (const entry of models) {
    const estimated = entry.estimated_requests ? "≈ " : "";
    body.append(
      el("tr", {}, [
        el("td", { class: "mono", text: entry.provider }),
        el("td", { class: "mono", text: entry.model }),
        el("td", { class: "mono", text: fmtCount(entry.requests) }),
        el("td", { class: "mono", text: estimated + fmtCount(entry.input_tokens) }),
        el("td", { class: "mono", text: estimated + fmtCount(entry.output_tokens) }),
        el("td", { class: "mono", text: fmtMs(entry.total_time_ms) }),
      ])
    );
  }
}

let statsTimer = null;

async function pollStats() {
  try {
    renderStats(await apiGet("/api/stats"));
    $("#lamp-proxy").className = "lamp on";
  } catch {
    $("#lamp-proxy").className = "lamp off";
  }
}

/* Traffic log */

const LOG_TTL_MS = 10 * 60 * 1000;
let logLastId = 0;
let logMaxLines = clampLogMax(Number(localStorage.getItem("llmproxy-log-max")));

function clampLogMax(value) {
  if (!Number.isFinite(value) || value <= 0) return 100;
  return Math.min(500, Math.max(20, value));
}

function logIsAtBottom(board) {
  return board.scrollHeight - board.scrollTop - board.clientHeight < 24;
}

function trimLogEntries(board) {
  const now = Date.now();
  for (const entry of [...board.querySelectorAll(".log-entry")]) {
    if (now - Number(entry.dataset.ts) >= LOG_TTL_MS) entry.remove();
    else break; // entries are chronological; the rest are younger
  }
  const entries = board.querySelectorAll(".log-entry");
  for (let i = 0; i < entries.length - logMaxLines; i += 1) entries[i].remove();
  $("#log-empty").hidden = board.querySelector(".log-entry") !== null;
}

function appendLogEntries(entries) {
  const board = $("#traffic-log");
  const stick = logIsAtBottom(board);
  for (const entry of entries) {
    const time =
      new Date(entry.ts_ms).toLocaleTimeString("en-GB") +
      "." +
      String(entry.ts_ms % 1000).padStart(3, "0");
    const node = el("div", { class: "log-entry" }, [
      el("span", { class: "log-time", text: `${time} ` }),
      el("span", {
        class: `log-${entry.class}-${entry.dir}`,
        text: `[${entry.class}-${entry.dir} ${entry.provider || "unknown"}] ${entry.text}`,
      }),
    ]);
    node.dataset.ts = entry.ts_ms;
    board.append(node);
  }
  trimLogEntries(board);
  if (stick && entries.length) board.scrollTop = board.scrollHeight;
}

let errLastId = 0;
const ERROR_BOARD_MAX = 200;

function appendErrorEntries(entries) {
  const board = $("#error-log");
  const stick = logIsAtBottom(board);
  for (const entry of entries) {
    const time =
      new Date(entry.ts_ms).toLocaleTimeString("en-GB") +
      "." +
      String(entry.ts_ms % 1000).padStart(3, "0");
    const source = entry.context ? `${entry.provider} · ${entry.context}` : entry.provider;
    board.append(
      el("div", { class: "log-entry" }, [
        el("span", { class: "log-time", text: `${time} ` }),
        el("span", { class: "log-error-src", text: `[${source}] ` }),
        el("span", { class: "log-error-msg", text: entry.text }),
      ])
    );
  }
  const rows = board.querySelectorAll(".log-entry");
  for (let i = 0; i < rows.length - ERROR_BOARD_MAX; i += 1) rows[i].remove();
  $("#error-empty").hidden = board.querySelector(".log-entry") !== null;
  if (stick && entries.length) board.scrollTop = board.scrollHeight;
}

async function pollErrors() {
  try {
    const data = await apiGet(`/api/errors?after=${errLastId}`);
    errLastId = data.latest_id ?? errLastId;
    appendErrorEntries(data.entries || []);
  } catch {
    /* older builds have no error endpoint; the proxy lamp reflects connectivity */
  }
}

async function pollLogs() {
  try {
    const data = await apiGet(`/api/logs?after=${logLastId}`);
    logLastId = data.latest_id ?? logLastId;
    appendLogEntries(data.entries || []);
    if (!$("#traffic-log").querySelector(".log-entry")) {
      $("#log-empty").textContent = "no traffic yet — entries appear as requests flow";
    }
  } catch (error) {
    // A 404 here means the running proxy predates the traffic log — say so
    // instead of sitting on an empty board forever.
    if (String(error.message || "").includes("unknown admin API endpoint")) {
      $("#log-empty").textContent =
        "this proxy build has no traffic log endpoint — rebuild llmproxy and restart it";
      $("#log-empty").hidden = false;
    }
  }
}

function startPolling() {
  if (statsTimer) clearInterval(statsTimer);
  statsTimer = setInterval(() => {
    if (!document.hidden) {
      pollStats();
      pollLogs();
      pollErrors();
    }
  }, 2000);
  pollStats();
  pollLogs();
  pollErrors();
}

async function loadMonitor() {
  try {
    const [overview, models, launch] = await Promise.all([
      apiGet("/api/overview"),
      apiGet("/api/models"),
      apiGet("/api/launch"),
    ]);
    launchData = launch;
    renderLaunch();
    renderRouting(models);

    proxyAddr = overview.proxy_addr || overview.listen_addr;
    if (catalogLoaded) renderCatalog();
    $("#meta-proxy-addr").textContent = proxyAddr;
    $("#meta-interceptor").textContent = overview.interceptor.enabled
      ? `interceptor on · ${overview.interceptor.model}`
      : "interceptor off";
    $("#footer-version").textContent = `llmproxy ${overview.version}` +
      (overview.launcher ? ` · ${overview.launcher} session` : "");
    $("#footer-config").textContent = overview.config_path || "no config file (defaults)";
    $("#footer-admin").textContent = overview.admin_addr ? `admin ${overview.admin_addr}` : "";
  } catch (error) {
    toast("Failed to load proxy state.", "error", String(error.message || error));
  }
}

/* Models tab */

let proxyAddr = null;
let catalogData = null;
let catalogLoaded = false;

/* Where a catalog entry came from; ranked for display order. */
function catalogSource(model) {
  if (model.proxy_route === "integrated") return "integrated";
  if (model.proxy_route === "namespaced") return "namespaced";
  if (model.proxy_target) return "alias";
  return "upstream";
}

const CATALOG_ORDER = { integrated: 0, alias: 1, upstream: 2, namespaced: 3 };

function renderCatalog() {
  $("#models-curl").textContent = `curl http://${proxyAddr || "…"}/v1/models`;
  const body = $("#catalog-table tbody");
  body.replaceChildren();
  if (!catalogData) return;
  const models = [...(catalogData.models || [])].sort((a, b) => {
    const rank = CATALOG_ORDER[catalogSource(a)] - CATALOG_ORDER[catalogSource(b)];
    return rank !== 0 ? rank : String(a.id).localeCompare(String(b.id));
  });
  const needle = $("#catalog-filter").value.trim().toLowerCase();
  let shown = 0;
  const counts = { integrated: 0, alias: 0, upstream: 0, namespaced: 0 };
  for (const model of models) {
    const source = catalogSource(model);
    counts[source] += 1;
    const provider = model.proxy_provider || "";
    const target = model.proxy_target
      || (source === "namespaced" ? String(model.id).slice(provider.length + 1) : model.id);
    const haystack = `${model.id} ${provider} ${target} ${source} ${model.proxy_tool || ""}`.toLowerCase();
    if (needle && !haystack.includes(needle)) continue;
    shown += 1;
    const cls = providerClasses[provider];
    body.append(
      el("tr", {}, [
        el("td", { class: "mono", text: model.id }),
        el("td", { class: "mono" }, [
          provider ? `${provider} · ${target} ` : `${target} `,
          cls ? el("span", { class: `badge badge-${cls}`, text: cls }) : null,
        ]),
        el("td", { class: "mono" }, [
          el("span", { class: "badge badge-neutral", text: source }),
          model.proxy_tool ? ` ${model.proxy_tool}` : null,
        ]),
      ])
    );
  }
  if (!shown) {
    body.append(el("tr", {}, [el("td", {
      colspan: "3",
      text: models.length
        ? "no models match the filter"
        : "no models yet — check provider connectivity under Providers",
    })]));
  }
  $("#catalog-count").textContent =
    `${shown} of ${models.length} ids · ${counts.integrated} integrated · ` +
    `${counts.alias} alias · ${counts.upstream} upstream · ${counts.namespaced} namespaced`;
}

async function loadModelsTab(force = false) {
  if (catalogLoaded && !force) {
    renderCatalog();
    return;
  }
  try {
    catalogData = await apiGet("/api/model-list");
    catalogLoaded = true;
  } catch (error) {
    toast("Failed to load the model catalog.", "error", String(error.message || error));
  }
  renderCatalog();
}

async function copyModelsCurl() {
  try {
    await navigator.clipboard.writeText($("#models-curl").textContent);
    const button = $("#copy-models-curl");
    button.textContent = "Copied";
    setTimeout(() => (button.textContent = "Copy"), 1300);
  } catch {
    toast("Copy failed — select the text manually.", "error");
  }
}

/* System prompt */

let promptMode = "intercept";
let sysEditorLoaded = false;
let sysUndoText = null;
const MAX_TEST_RESULTS = 5;
/* Unsaved editor text per mode, kept across mode switches and tab revisits. */
const sysDrafts = { intercept: null, integrated: null };

const MODE_EXPLAINERS = {
  intercept:
    "One decision: answer simple deterministic requests locally, or PASS to forward the untouched request to normal external routing.",
  integrated:
    "Three-way routing: answer simple requests locally, hand everything else to the provider route whose hint claims it, or ROUTE_DEFAULT when none fits.",
};

const TEST_EXAMPLES = [
  {
    label: "deterministic edit",
    prompt: 'Convert this JSON to YAML: {"name": "llmproxy", "ports": [4000, 11180]}',
  },
  {
    label: "GUI design",
    prompt: "Design a settings page for the admin dashboard and implement the layout in HTML and CSS.",
  },
  {
    label: "server debugging",
    prompt: "Investigate why POST /v1/chat/completions intermittently returns HTTP 500 and propose a fix.",
  },
  {
    label: "deep exploration",
    prompt: "Explore the repository structure and plan a migration of the storage layer to async I/O.",
  },
];

function savedPrompt(mode) {
  return mode === "integrated"
    ? configData.integrated.instructions
    : configData.interceptor.instructions;
}

function promptIsDefault(mode) {
  return mode === "integrated"
    ? configData.integrated.instructions_are_default
    : configData.interceptor.instructions_are_default;
}

function stashDraft() {
  const editor = $("#sys-editor");
  sysDrafts[promptMode] = editor.value === editor.dataset.saved ? null : editor.value;
}

function fillSysEditor() {
  const editor = $("#sys-editor");
  const saved = savedPrompt(promptMode);
  editor.dataset.saved = saved;
  editor.value = sysDrafts[promptMode] ?? saved;
  sysUndoText = null;
  $("#revise-undo").hidden = true;
  renderModeChrome();
  updateSysStatus();
  updateSysDirty();
}

function renderModeChrome() {
  $("#mode-intercept").setAttribute("aria-pressed", String(promptMode === "intercept"));
  $("#mode-integrated").setAttribute("aria-pressed", String(promptMode === "integrated"));
  $("#mode-explainer").textContent = MODE_EXPLAINERS[promptMode];
  $("#test-run").textContent = `Test in ${promptMode} mode`;
  const routesBlock = $("#sys-routes-block");
  routesBlock.hidden = promptMode !== "integrated";
  if (promptMode === "integrated") renderPromptRoutes();
}

function renderPromptRoutes() {
  const list = $("#sys-routes");
  list.replaceChildren();
  const targets = Object.entries(configData?.integrated?.targets || {}).sort(([a], [b]) =>
    a.localeCompare(b)
  );
  for (const [name, target] of targets) {
    list.append(
      el("li", {}, [
        el("span", { class: "route-name mono", text: name }),
        el("span", {
          class: "route-dest mono",
          text: `${target.provider}/${target.model} · ${target.reasoning_effort || "provider-default"}`,
        }),
        el("span", { class: "route-hint", text: target.hint }),
      ])
    );
  }
  if (!targets.length) {
    list.append(
      el("li", {}, [
        el("span", {
          class: "route-hint",
          text: "no targets configured — integrated routing has nowhere to send work",
        }),
      ])
    );
  }
}

function setPromptMode(mode) {
  if (mode === promptMode) return;
  stashDraft();
  promptMode = mode;
  fillSysEditor();
}

function updateSysStatus() {
  const interceptor = configData.interceptor;
  const modeState =
    promptMode === "integrated"
      ? configData.integrated.enabled
        ? "on"
        : "off — launchers enable it per session"
      : interceptor.enabled
        ? "on"
        : "off";
  $("#sys-status").textContent =
    `triage ${interceptor.provider} · ${interceptor.model} · ${promptMode} ${modeState}`;
}

function updateSysDirty() {
  const editor = $("#sys-editor");
  if (editor.value !== editor.dataset.saved) {
    $("#sys-dirty").textContent =
      "unsaved changes — route tests use this text; press Save to persist";
  } else if (promptIsDefault(promptMode)) {
    $("#sys-dirty").textContent = `built-in ${promptMode} policy`;
  } else {
    $("#sys-dirty").textContent = `saved custom ${promptMode} policy`;
  }
}

async function loadPromptTab() {
  try {
    if (!configData) configData = await apiGet("/api/config");
    if (sysEditorLoaded) stashDraft();
    fillSysEditor();
    sysEditorLoaded = true;
  } catch (error) {
    toast("Failed to load the routing policy.", "error", String(error.message || error));
  }
}

async function saveSystemPrompt(text, what) {
  const endpoint = promptMode === "integrated" ? "/api/integrated" : "/api/interceptor";
  try {
    const result = await apiSend(endpoint, "PUT", { instructions: text });
    applyToast(result, what);
    configData = result.config;
    sysDrafts[promptMode] = null;
    fillSysEditor();
    renderConfigEditors();
  } catch (error) {
    toast(`${what} was not saved.`, "error", error.message);
  }
}

function verdictLabel(verdict) {
  switch (verdict.decision) {
    case "local":
      return "→ local";
    case "external":
      return "→ external (PASS)";
    case "default_route":
      return "→ default route";
    case "redirect":
      return verdict.target ? `→ ${verdict.target}` : "→ redirect";
    case "invalid":
      return "invalid output";
    default:
      return verdict.decision || "error";
  }
}

function renderTestResult(prompt, verdict) {
  const host = $("#test-results");
  const meta = [
    verdict.mode && `${verdict.mode} mode`,
    verdict.model && `${verdict.provider} / ${verdict.model}`,
    verdict.elapsed_ms !== undefined && `${verdict.elapsed_ms} ms`,
    `“${prompt.length > 90 ? prompt.slice(0, 90) + "…" : prompt}”`,
  ]
    .filter(Boolean)
    .join(" · ");
  const card = el("div", { class: `test-result verdict-${verdict.decision}` }, [
    el("div", { class: "test-verdict mono", text: verdictLabel(verdict) }),
    verdict.note ? el("div", { class: "test-note", text: verdict.note }) : null,
    el("div", { class: "test-meta mono", text: meta }),
    verdict.detail
      ? el("div", { class: "test-detail", text: `local model said: ${verdict.detail}` })
      : null,
  ]);
  host.prepend(card);
  while (host.children.length > MAX_TEST_RESULTS) host.lastChild.remove();
}

async function runRouteTest() {
  const prompt = $("#test-prompt").value.trim();
  if (!prompt) {
    toast("Type a test prompt first.", "warn");
    return;
  }
  const button = $("#test-run");
  button.disabled = true;
  $("#test-running").hidden = false;
  try {
    const verdict = await apiSend("/api/interceptor/test", "POST", {
      prompt,
      instructions: $("#sys-editor").value,
      mode: promptMode,
    });
    renderTestResult(prompt, verdict);
  } catch (error) {
    toast("Route test failed.", "error", error.message);
  } finally {
    button.disabled = false;
    $("#test-running").hidden = true;
  }
}

function renderTestChips() {
  const row = $("#test-chips");
  row.replaceChildren();
  for (const example of TEST_EXAMPLES) {
    row.append(
      el("button", {
        class: "chip",
        type: "button",
        text: example.label,
        title: example.prompt,
        onclick: () => {
          $("#test-prompt").value = example.prompt;
          $("#test-prompt").focus();
        },
      })
    );
  }
}

async function runRevision() {
  const request = $("#revise-input").value.trim();
  if (!request) {
    toast("Describe the change you want first.", "warn");
    return;
  }
  const button = $("#revise-run");
  button.disabled = true;
  $("#revise-running").hidden = false;
  try {
    const result = await apiSend("/api/interceptor/revise", "POST", {
      request,
      instructions: $("#sys-editor").value,
      mode: promptMode,
    });
    sysUndoText = $("#sys-editor").value;
    $("#sys-editor").value = result.instructions;
    $("#revise-undo").hidden = false;
    updateSysDirty();
    $("#sys-dirty").textContent =
      "revised by the local model — review the text, then press Save to persist";
    toast("Revision ready — review the editor, then save.");
  } catch (error) {
    toast("Revision failed.", "error", error.message);
  } finally {
    button.disabled = false;
    $("#revise-running").hidden = true;
  }
}

/* Configuration editors */

let configData = null;

function providerOptions(select, selected, includeDefaultChoice) {
  select.replaceChildren();
  if (includeDefaultChoice) {
    select.append(el("option", { value: "", text: "(default provider)" }));
  }
  for (const name of Object.keys(configData?.providers || {}).sort()) {
    const option = el("option", { value: name, text: name });
    if (name === selected) option.selected = true;
    select.append(option);
  }
  if (includeDefaultChoice && !selected) select.value = "";
}

function renderAliasEditor() {
  const tbody = $("#cfg-alias-table tbody");
  tbody.replaceChildren();
  const aliases = Object.entries(configData.model_aliases).sort(([a], [b]) => a.localeCompare(b));
  for (const [name, route] of aliases) {
    const modelInput = el("input", { class: "input mono", value: route.model, "aria-label": `Model for ${name}` });
    const providerSelect = el("select", { class: "input mono", "aria-label": `Provider for ${name}` });
    providerOptions(providerSelect, route.provider, true);
    const save = el("button", { class: "button", type: "button", text: "Save" });
    save.addEventListener("click", async () => {
      try {
        const result = await apiSend(`/api/aliases/${encodeURIComponent(name)}`, "PUT", {
          model: modelInput.value.trim(),
          provider: providerSelect.value || null,
        });
        applyToast(result, `Alias ${name}`);
        refreshFromResult(result);
      } catch (error) {
        toast(`Alias ${name} was not saved.`, "error", error.message);
      }
    });
    const remove = el("button", { class: "button button-danger", type: "button", text: "Delete" });
    remove.addEventListener("click", async () => {
      if (!confirm(`Delete alias ${name}?`)) return;
      try {
        const result = await apiSend(`/api/aliases/${encodeURIComponent(name)}`, "DELETE");
        applyToast(result, `Alias ${name} removed;`);
        refreshFromResult(result);
      } catch (error) {
        toast(`Could not delete ${name}.`, "error", error.message);
      }
    });
    tbody.append(
      el("tr", {}, [
        el("td", { text: name }),
        el("td", {}, [modelInput]),
        el("td", {}, [providerSelect]),
        el("td", {}, [el("div", { class: "row-actions" }, [save, remove])]),
      ])
    );
  }
  if (!aliases.length) {
    tbody.append(el("tr", {}, [el("td", { colspan: "4", text: "no aliases yet — add one below" })]));
  }
  providerOptions($("#alias-add-provider"), "", true);
}

function renderInterceptorForm() {
  const interceptor = configData.interceptor;
  $("#int-enabled").checked = interceptor.enabled;
  providerOptions($("#int-provider"), interceptor.provider, false);
  $("#int-model").value = interceptor.model;
  $("#int-timeout").value = interceptor.timeout_ms;
  $("#int-temperature").value = interceptor.temperature;
  $("#int-max-tokens").value = interceptor.max_tokens;
  $("#int-fallback").checked = interceptor.fallback_on_error;
  $("#int-parallel").checked = interceptor.parallel_tool_calls;
}

const REASONING_EFFORT_VALUES = ["", "none", "minimal", "low", "medium", "high", "xhigh", "max"];

function populateReasoningEffortOptions(select, selected, emptyLabel) {
  select.replaceChildren(...REASONING_EFFORT_VALUES.map((value) =>
    el("option", { value, text: value || emptyLabel })
  ));
  select.value = selected || "";
}

function constrainReasoningEffort(select, kind) {
  const copilot = kind === "copilot";
  const anthropic = kind === "anthropic";
  select.disabled = copilot;
  for (const option of select.options) {
    option.disabled = anthropic && (option.value === "none" || option.value === "minimal");
  }
  if (copilot || select.selectedOptions[0]?.disabled) select.value = "";
}

function configuredProviderKind(name) {
  return configData?.providers?.[name]?.kind || "openai-compatible";
}

function appendIntegratedTargetRow(name, target) {
  const tbody = $("#integrated-targets tbody");
  tbody.querySelector(".integrated-empty")?.remove();
  const targetName = el("input", { class: "input mono target-name", value: name, "aria-label": "Target tool name" });
  const provider = el("select", { class: "input mono target-provider", "aria-label": `Provider for ${name}` });
  providerOptions(provider, target.provider, false);
  const model = el("input", { class: "input mono target-model", value: target.model, "aria-label": `Model for ${name}` });
  const effort = el("select", { class: "input mono target-effort", "aria-label": `Reasoning effort for ${name}` });
  populateReasoningEffortOptions(effort, target.reasoning_effort, "provider default");
  const syncEffort = () => constrainReasoningEffort(effort, configuredProviderKind(provider.value));
  provider.addEventListener("change", syncEffort);
  syncEffort();
  const hint = el("input", { class: "input target-hint", value: target.hint, "aria-label": `Routing hint for ${name}` });
  const remove = el("button", {
    class: "button button-danger",
    type: "button",
    text: "Remove",
    onclick: (event) => event.target.closest("tr").remove(),
  });
  tbody.append(el("tr", {}, [
    el("td", {}, [targetName]),
    el("td", {}, [provider]),
    el("td", {}, [model]),
    el("td", {}, [effort]),
    el("td", {}, [hint]),
    el("td", {}, [remove]),
  ]));
}

function renderIntegratedForm() {
  const integrated = configData.integrated;
  $("#integrated-enabled").checked = integrated.enabled;
  $("#integrated-ttl").value = integrated.sticky_ttl_secs;
  $("#integrated-capacity").value = integrated.sticky_capacity;
  const tbody = $("#integrated-targets tbody");
  tbody.replaceChildren();
  for (const [name, target] of Object.entries(integrated.targets).sort(([a], [b]) => a.localeCompare(b))) {
    appendIntegratedTargetRow(name, target);
  }
  if (!tbody.children.length) {
    tbody.append(el("tr", { class: "integrated-empty" }, [el("td", { colspan: "6", text: "no redirect targets configured" })]));
  }
  providerOptions($("#integrated-add-provider"), "", false);
  const addEffort = $("#integrated-add-effort");
  populateReasoningEffortOptions(addEffort, "", "provider default");
  const syncAddEffort = () => constrainReasoningEffort(
    addEffort,
    configuredProviderKind($("#integrated-add-provider").value)
  );
  $("#integrated-add-provider").onchange = syncAddEffort;
  syncAddEffort();
}

function collectIntegratedTargets() {
  const targets = {};
  for (const row of document.querySelectorAll("#integrated-targets tbody tr:not(.integrated-empty)")) {
    const name = row.querySelector(".target-name").value.trim();
    const provider = row.querySelector(".target-provider").value;
    const model = row.querySelector(".target-model").value.trim();
    const reasoningEffort = row.querySelector(".target-effort").value;
    const hint = row.querySelector(".target-hint").value.trim();
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(name) || name === "PASS" || name === "ROUTE_DEFAULT") {
      throw new Error(`${name || "target"}: use a non-reserved 1-64 character tool name containing letters, digits, _, or -`);
    }
    if (!provider || !model || !hint || hint.includes("\n")) throw new Error(`${name}: provider, model, and one-line hint are required`);
    if (targets[name]) throw new Error(`duplicate integrated target ${name}`);
    targets[name] = { provider, model, reasoning_effort: reasoningEffort || null, hint };
  }
  return targets;
}

function renderProviderEditors() {
  const host = $("#provider-editors");
  const wasOpen = new Set(
    [...host.querySelectorAll("details.provider-editor[open]")].map((node) => node.dataset.name)
  );
  host.replaceChildren();
  const providers = Object.entries(configData.providers).sort(([a], [b]) => a.localeCompare(b));
  for (const [name, provider] of providers) {
    const kind = el("select", { class: "input mono", "aria-label": `Kind for ${name}` }, [
      "openai-compatible", "openai", "anthropic", "copilot",
    ].map((value) => el("option", { value, text: value })));
    kind.value = provider.kind;
    const reasoningEffort = el("select", {
      class: "input mono", "aria-label": `Reasoning effort for ${name}`,
      title: "Force this effort on every generation routed to the provider",
    });
    populateReasoningEffortOptions(reasoningEffort, provider.reasoning_effort, "not enforced");
    const syncReasoningEffort = () => {
      constrainReasoningEffort(reasoningEffort, kind.value);
    };
    kind.addEventListener("change", syncReasoningEffort);
    syncReasoningEffort();
    const baseUrl = el("input", { class: "input mono", value: provider.base_url, "aria-label": `Base URL for ${name}` });
    const apiKeyEnv = el("input", {
      class: "input mono", value: provider.api_key_env || "",
      placeholder: "env var name", "aria-label": `API key env for ${name}`,
    });
    const apiKey = el("input", {
      class: "input mono", type: "password", autocomplete: "off",
      placeholder: provider.api_key_set ? "unchanged (key stored)" : "no key set",
      "aria-label": `API key for ${name}`,
    });
    const timeout = el("input", { class: "input mono", type: "number", min: "1", value: provider.timeout_secs });
    const connectTimeout = el("input", { class: "input mono", type: "number", min: "1", value: provider.connect_timeout_secs });

    const save = el("button", { class: "button button-primary", type: "button", text: "Save" });
    save.addEventListener("click", async () => {
      const body = {
        kind: kind.value,
        base_url: baseUrl.value.trim(),
        reasoning_effort: reasoningEffort.value,
        timeout_secs: Number(timeout.value) || provider.timeout_secs,
        connect_timeout_secs: Number(connectTimeout.value) || provider.connect_timeout_secs,
      };
      const envValue = apiKeyEnv.value.trim();
      if (envValue !== (provider.api_key_env || "")) body.api_key_env = envValue;
      if (apiKey.value) body.api_key = apiKey.value;
      try {
        const result = await apiSend(`/api/providers/${encodeURIComponent(name)}`, "PUT", body);
        applyToast(result, `Provider ${name}`);
        refreshFromResult(result);
      } catch (error) {
        toast(`Provider ${name} was not saved.`, "error", error.message);
      }
    });

    const keyBadge = provider.api_key_set
      ? el("span", { class: "badge badge-neutral", text: "key stored" })
      : provider.api_key_env
        ? el("span", { class: "badge badge-neutral", text: `env ${provider.api_key_env}` })
        : null;
    const summary = el("summary", { class: "provider-summary" }, [
      el("span", { class: "provider-name mono", text: name }),
      el("span", { class: `badge badge-${provider.class}`, text: provider.class }),
      provider.is_default ? el("span", { class: "badge badge-neutral", text: "default" }) : null,
      provider.from_upstream_section ? el("span", { class: "badge badge-neutral", text: "[upstream]" }) : null,
      provider.reasoning_effort ? el("span", { class: "badge badge-neutral", text: `reasoning ${provider.reasoning_effort}` }) : null,
      keyBadge,
      el("span", { class: "provider-url", text: provider.base_url }),
    ]);

    const dangerActions = [];
    if (provider.api_key_set) {
      const clearKey = el("button", { class: "button button-danger", type: "button", text: "Clear stored key" });
      clearKey.addEventListener("click", async () => {
        if (!confirm(`Remove the stored API key for ${name}?`)) return;
        try {
          const result = await apiSend(`/api/providers/${encodeURIComponent(name)}`, "PUT", { api_key: "" });
          applyToast(result, `Provider ${name} key cleared;`);
          refreshFromResult(result);
        } catch (error) {
          toast(`Could not clear the key for ${name}.`, "error", error.message);
        }
      });
      dangerActions.push(clearKey);
    }
    if (!provider.is_default && !provider.from_upstream_section) {
      const remove = el("button", { class: "button button-danger", type: "button", text: "Delete provider" });
      remove.addEventListener("click", async () => {
        if (!confirm(`Delete provider ${name}? Aliases pointing at it will block the change.`)) return;
        try {
          const result = await apiSend(`/api/providers/${encodeURIComponent(name)}`, "DELETE");
          applyToast(result, `Provider ${name} removed;`);
          refreshFromResult(result);
        } catch (error) {
          toast(`Could not delete ${name}.`, "error", error.message);
        }
      });
      dangerActions.push(remove);
    }

    const card = el("details", { class: "provider-editor", "data-name": name }, [
      summary,
      el("div", { class: "field-row" }, [
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "kind" }), kind]),
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "base_url" }), baseUrl]),
      ]),
      el("div", { class: "field-row" }, [
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "api_key_env" }), apiKeyEnv]),
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "api_key" }), apiKey]),
      ]),
      el("div", { class: "field-row" }, [
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "reasoning_effort" }), reasoningEffort]),
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "timeout_secs" }), timeout]),
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "connect_timeout_secs" }), connectTimeout]),
      ]),
      el("div", { class: "provider-actions" }, [...dangerActions, el("span", { class: "spacer" }), save]),
    ]);
    if (wasOpen.has(name)) card.open = true;
    host.append(card);
  }
}

function renderRoleMapRows(roleMap) {
  const host = $("#rolemap-rows");
  host.replaceChildren();
  for (const [from, to] of Object.entries(roleMap).sort()) {
    const row = el("div", { class: "rolemap-row", "data-from": from }, [
      el("span", { class: "mono", text: from }),
      el("span", { class: "rolemap-arrow", text: "→" }),
      el("span", { class: "mono", text: to }),
      el("button", {
        class: "button button-danger", type: "button", text: "Remove",
        onclick: (event) => event.target.closest(".rolemap-row").remove(),
      }),
    ]);
    row.dataset.to = to;
    host.append(row);
  }
}

function collectRoleMap() {
  const map = {};
  for (const row of document.querySelectorAll("#rolemap-rows .rolemap-row")) {
    map[row.dataset.from] = row.dataset.to;
  }
  return map;
}

function renderSettingsForm() {
  providerOptions($("#set-default-provider"), configData.default_provider, false);
  $("#set-log-retention").value = configData.max_logging_retention_days;
  $("#set-responses-mode").value = configData.responses.mode;
  $("#set-disable-thinking").checked = configData.chat.disable_thinking;
  $("#set-expose-aliases").checked = configData.models.expose_aliases;
  $("#set-expose-upstream").checked = configData.models.expose_upstream;
  renderRoleMapRows(configData.role_map);
}

function renderConfigEditors() {
  if (!configData) return;
  renderAliasEditor();
  renderInterceptorForm();
  renderIntegratedForm();
  renderProviderEditors();
  renderSettingsForm();
}

function refreshFromResult(result) {
  if (result?.config) {
    configData = result.config;
    renderConfigEditors();
  }
  loadMonitor();
}

let configEditorsRendered = false;

async function loadConfigEditors(force = false) {
  if (configEditorsRendered && !force) return;
  try {
    if (!configData || force) configData = await apiGet("/api/config");
    renderConfigEditors();
    configEditorsRendered = true;
  } catch (error) {
    toast("Failed to load the configuration.", "error", String(error.message || error));
  }
}

let rawConfigLoaded = false;

async function loadRawConfig() {
  try {
    const raw = await apiGet("/api/config/raw");
    const editor = $("#raw-toml");
    if (!raw.editable) {
      editor.value = "";
      editor.disabled = true;
      $("#raw-apply").disabled = true;
      $("#raw-reload").disabled = true;
      $("#raw-intro").textContent =
        "No config file was loaded at startup, so there is nothing to edit here. " +
        "Start llmproxy with --config to enable the file editor.";
      rawConfigLoaded = true;
      return;
    }
    editor.value = raw.toml;
    editor.disabled = false;
    $("#raw-apply").disabled = false;
    $("#raw-reload").disabled = false;
    rawConfigLoaded = true;
  } catch (error) {
    toast("Failed to read the config file.", "error", error.message);
  }
}

/* Initialization */

document.addEventListener("DOMContentLoaded", () => {
  renderTab();
  loadMonitor();
  startPolling();
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) pollStats();
  });

  for (const radio of document.querySelectorAll('#agent-choice input, #style-choice input')) {
    radio.addEventListener("change", renderLaunch);
  }
  $("#copy-launch").addEventListener("click", copyLaunchCommand);

  $("#reset-stats").addEventListener("click", async () => {
    try {
      await apiSend("/api/stats/reset", "POST");
      toast("Counters reset.");
      pollStats();
    } catch (error) {
      toast("Reset failed.", "error", error.message);
    }
  });

  const logSlider = $("#log-max-lines");
  logSlider.value = String(logMaxLines);
  $("#log-max-value").textContent = String(logMaxLines);
  logSlider.addEventListener("input", () => {
    logMaxLines = clampLogMax(Number(logSlider.value));
    $("#log-max-value").textContent = String(logMaxLines);
    localStorage.setItem("llmproxy-log-max", String(logMaxLines));
    trimLogEntries($("#traffic-log"));
  });

  $("#clear-logs").addEventListener("click", async () => {
    try {
      await apiSend("/api/logs/clear", "POST");
      toast("Traffic log cleared.");
    } catch (error) {
      toast("Could not clear the server-side log.", "error", error.message);
    }
    const board = $("#traffic-log");
    for (const entry of board.querySelectorAll(".log-entry")) entry.remove();
    $("#log-empty").hidden = false;
  });

  $("#clear-errors").addEventListener("click", async () => {
    try {
      await apiSend("/api/errors/clear", "POST");
      toast("Errors cleared.");
    } catch (error) {
      toast("Could not clear the server-side errors.", "error", error.message);
    }
    const board = $("#error-log");
    for (const entry of board.querySelectorAll(".log-entry")) entry.remove();
    $("#error-empty").hidden = false;
  });

  $("#alias-add-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const name = $("#alias-add-name").value.trim();
    try {
      const result = await apiSend(`/api/aliases/${encodeURIComponent(name)}`, "PUT", {
        model: $("#alias-add-model").value.trim(),
        provider: $("#alias-add-provider").value || null,
      });
      applyToast(result, `Alias ${name}`);
      $("#alias-add-name").value = "";
      $("#alias-add-model").value = "";
      refreshFromResult(result);
    } catch (error) {
      toast(`Alias ${name} was not added.`, "error", error.message);
    }
  });

  $("#interceptor-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const body = {
      enabled: $("#int-enabled").checked,
      provider: $("#int-provider").value,
      model: $("#int-model").value.trim(),
      timeout_ms: Number($("#int-timeout").value) || undefined,
      fallback_on_error: $("#int-fallback").checked,
      temperature: Number($("#int-temperature").value),
      max_tokens: Math.max(0, Math.trunc(Number($("#int-max-tokens").value)) || 0),
      parallel_tool_calls: $("#int-parallel").checked,
    };
    try {
      const result = await apiSend("/api/interceptor", "PUT", body);
      applyToast(result, "Interceptor");
      refreshFromResult(result);
    } catch (error) {
      toast("Interceptor settings were not saved.", "error", error.message);
    }
  });

  $("#integrated-add").addEventListener("click", () => {
    const name = $("#integrated-add-name").value.trim();
    const provider = $("#integrated-add-provider").value;
    const model = $("#integrated-add-model").value.trim();
    const reasoningEffort = $("#integrated-add-effort").value;
    const hint = $("#integrated-add-hint").value.trim();
    if (!name || !provider || !model || !hint) {
      toast("Target name, provider, model, and hint are required.", "error");
      return;
    }
    appendIntegratedTargetRow(name, { provider, model, reasoning_effort: reasoningEffort || null, hint });
    $("#integrated-add-name").value = "";
    $("#integrated-add-model").value = "";
    $("#integrated-add-effort").value = "";
    $("#integrated-add-hint").value = "";
  });

  $("#integrated-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      const result = await apiSend("/api/integrated", "PUT", {
        enabled: $("#integrated-enabled").checked,
        sticky_ttl_secs: Number($("#integrated-ttl").value),
        sticky_capacity: Number($("#integrated-capacity").value),
        targets: collectIntegratedTargets(),
      });
      applyToast(result, "Integrated routing");
      refreshFromResult(result);
    } catch (error) {
      toast("Integrated routing was not saved.", "error", error.message);
    }
  });

  $("#provider-add-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const name = $("#provider-add-name").value.trim();
    try {
      const result = await apiSend(`/api/providers/${encodeURIComponent(name)}`, "PUT", {
        kind: $("#provider-add-kind").value,
        base_url: $("#provider-add-url").value.trim(),
      });
      applyToast(result, `Provider ${name}`);
      $("#provider-add-name").value = "";
      $("#provider-add-url").value = "";
      refreshFromResult(result);
    } catch (error) {
      toast(`Provider ${name} was not added.`, "error", error.message);
    }
  });

  $("#rolemap-add").addEventListener("click", () => {
    const from = $("#rolemap-add-from").value.trim();
    const to = $("#rolemap-add-to").value.trim();
    if (!from || !to) return;
    const map = collectRoleMap();
    map[from] = to;
    renderRoleMapRows(map);
    $("#rolemap-add-from").value = "";
    $("#rolemap-add-to").value = "";
  });

  $("#settings-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      const result = await apiSend("/api/settings", "PUT", {
        default_provider: $("#set-default-provider").value,
        max_logging_retention_days: Math.max(0, Math.trunc(Number($("#set-log-retention").value)) || 0),
        responses_mode: $("#set-responses-mode").value,
        disable_thinking: $("#set-disable-thinking").checked,
        expose_aliases: $("#set-expose-aliases").checked,
        expose_upstream: $("#set-expose-upstream").checked,
        role_map: collectRoleMap(),
      });
      applyToast(result, "Settings");
      refreshFromResult(result);
    } catch (error) {
      toast("Settings were not saved.", "error", error.message);
    }
  });

  $("#catalog-filter").addEventListener("input", renderCatalog);
  $("#catalog-refresh").addEventListener("click", () => loadModelsTab(true));
  $("#copy-models-curl").addEventListener("click", copyModelsCurl);

  $("#mode-intercept").addEventListener("click", () => setPromptMode("intercept"));
  $("#mode-integrated").addEventListener("click", () => setPromptMode("integrated"));
  renderTestChips();
  $("#sys-editor").addEventListener("input", updateSysDirty);
  $("#sys-save").addEventListener("click", () =>
    saveSystemPrompt($("#sys-editor").value, `The ${promptMode} prompt`));
  $("#sys-reset").addEventListener("click", () => {
    if (confirm(`Replace the ${promptMode} policy with the built-in default?`)) {
      saveSystemPrompt("", `The ${promptMode} prompt reset;`);
    }
  });
  $("#test-run").addEventListener("click", runRouteTest);
  $("#test-prompt").addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") runRouteTest();
  });
  $("#revise-run").addEventListener("click", runRevision);
  $("#revise-undo").addEventListener("click", () => {
    if (sysUndoText !== null) {
      $("#sys-editor").value = sysUndoText;
      sysUndoText = null;
      $("#revise-undo").hidden = true;
      updateSysDirty();
    }
  });

  $("#raw-reload").addEventListener("click", loadRawConfig);
  $("#raw-config-disclosure").addEventListener("toggle", (event) => {
    if (event.currentTarget.open && !rawConfigLoaded) loadRawConfig();
  });
  $("#raw-apply").addEventListener("click", async () => {
    try {
      const result = await apiSend("/api/config/raw", "PUT", { toml: $("#raw-toml").value });
      applyToast(result, "Config file");
      refreshFromResult(result);
      await loadRawConfig();
    } catch (error) {
      toast("Config was not applied.", "error", error.message);
    }
  });
});
