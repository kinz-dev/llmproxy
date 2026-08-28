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

const SVG_NS = "http://www.w3.org/2000/svg";

/* el() builds HTML; SVG children need the namespaced constructor. */
function svgEl(tag, attrs = {}) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [key, val] of Object.entries(attrs)) {
    if (key === "class") node.setAttribute("class", val);
    else if (key === "text") node.textContent = val;
    else if (val !== null && val !== undefined) node.setAttribute(key, val);
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
const ACTIVITY_VIEWS = ["usage", "spend", "decisions", "shadow", "logs"];
const ROUTING_VIEWS = ["triage", "policy", "memory"];

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
  if (primary === "activity" && !ACTIVITY_VIEWS.includes(secondary)) secondary = "usage";
  if (primary === "routing" && !ROUTING_VIEWS.includes(secondary)) secondary = "triage";
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
  renderSubview("activity", primary === "activity" ? secondary : "usage", ACTIVITY_VIEWS);
  renderSubview("routing", primary === "routing" ? secondary : "triage", ROUTING_VIEWS);
  document.title = `llmproxy · ${primary}`;

  if (primary === "models") {
    loadModelsTab();
    loadConfigEditors();
  } else if (["routing", "providers", "settings"].includes(primary)) {
    loadConfigEditors();
  }
  if (primary === "routing" && secondary === "policy") loadPromptTab();
  if (primary === "routing" && secondary === "memory") loadMemory();
  if (primary === "activity" && secondary === "spend") loadSpend();
  if (primary === "activity" && secondary === "shadow") pollShadow();
  if (primary === "settings") loadHealth();
}

window.addEventListener("hashchange", () => {
  renderTab();
  window.scrollTo(0, 0);
});

/* Money, evidence, and memory
   ---------------------------
   Every figure here carries its own caveat next to it. Spend priced from
   estimated token counts is an approximation, a subscription provider has no
   dollar figure at all, and a rank from a handful of requests is noise: each
   is said in the panel rather than left for the reader to infer. */

function fmtMoney(value, currency) {
  if (value === null || value === undefined) return "—";
  const symbol = currency === "USD" ? "$" : "";
  const suffix = currency && currency !== "USD" ? ` ${currency}` : "";
  const digits = Math.abs(value) >= 100 ? 2 : Math.abs(value) >= 1 ? 3 : 4;
  return `${symbol}${value.toFixed(digits)}${suffix}`;
}

function fmtPercent(value) {
  return value === null || value === undefined ? "—" : `${(value * 100).toFixed(1)}%`;
}

let spendData = null;
let leaderboardData = null;

function spendQuery() {
  const month = $("#spend-month").value;
  const project = $("#spend-project").value;
  const params = new URLSearchParams();
  if (month) params.set("month", month);
  if (project) params.set("project", project);
  const query = params.toString();
  return query ? `?${query}` : "";
}

async function loadSpend() {
  try {
    const query = spendQuery();
    const [spend, leaderboard] = await Promise.all([
      apiGet(`/api/spend${query}`),
      apiGet(`/api/leaderboard${query}`),
    ]);
    spendData = spend;
    leaderboardData = leaderboard;
    renderSpend();
    renderLeaderboard();
  } catch (error) {
    toast("Failed to load spend.", "error", String(error.message || error));
  }
}

function fillSelect(select, values, current, allLabel) {
  const previous = current ?? select.value;
  select.replaceChildren(el("option", { value: "", text: allLabel }));
  for (const value of values) select.append(el("option", { value, text: value }));
  select.value = values.includes(previous) ? previous : "";
}

function renderSpend() {
  const data = spendData;
  if (!data) return;
  if (data.available === false) {
    $("#spend-intro").textContent = data.reason || "usage history is not being recorded";
    return;
  }
  fillSelect($("#spend-month"), data.months || [], data.month, "all months");
  fillSelect(
    $("#spend-project"),
    (data.projects || []).map((row) => row.project),
    data.project,
    "all projects"
  );

  const total = data.total || {};
  const cost = total.cost || {};
  const currency = data.currency;
  const priced = data.priced;
  $("#spend-total").textContent = priced ? fmtMoney(cost.total, currency) : "—";
  $("#spend-total-sub").textContent = `${fmtCount(total.requests || 0)} requests`;
  $("#spend-input").textContent = priced ? fmtMoney(cost.input, currency) : "—";
  $("#spend-input-sub").textContent = `${fmtCount(total.input_tokens || 0)} prompt tokens`;
  $("#spend-cache").textContent = priced ? fmtMoney(cost.cache_read, currency) : "—";
  $("#spend-cache-sub").textContent = `${fmtCount(total.cache_read_tokens || 0)} read · ${fmtCount(total.cache_write_tokens || 0)} written`;
  $("#spend-output").textContent = priced ? fmtMoney(cost.output, currency) : "—";
  $("#spend-output-sub").textContent = `${fmtCount(total.output_tokens || 0)} completion tokens`;

  const caveats = [];
  if (!priced) caveats.push("No prices are configured, so only token volume is shown. Add a [providers.*.pricing] table.");
  if (total.estimated_requests) {
    caveats.push(`${fmtCount(total.estimated_requests)} request(s) had their tokens estimated rather than reported, so the figure is an approximation.`);
  }
  if (total.subscription_requests) {
    caveats.push(`${fmtCount(total.subscription_requests)} request(s) went to a subscription provider and are billed against a plan, not per token.`);
  }
  if (total.unpriced_requests) {
    caveats.push(`${fmtCount(total.unpriced_requests)} request(s) used a model with no configured price and are excluded from the total.`);
  }
  $("#spend-caveat").textContent = caveats.join(" ");

  const projectRows = $("#spend-projects tbody");
  projectRows.replaceChildren();
  for (const row of data.projects || []) {
    projectRows.append(el("tr", {}, [
      el("td", { class: "mono", text: row.project }),
      el("td", { class: "mono", text: fmtCount(row.requests) }),
      el("td", { class: "mono", text: fmtCount(row.input_tokens) }),
      el("td", { class: "mono", text: fmtCount(row.output_tokens) }),
      el("td", { class: "mono", text: row.priced_requests ? fmtMoney(row.cost.total, currency) : "—" }),
    ]));
  }
  if (!projectRows.children.length) {
    projectRows.append(el("tr", {}, [el("td", { colspan: "5", text: "nothing recorded yet" })]));
  }

  const modelRows = $("#spend-models tbody");
  modelRows.replaceChildren();
  for (const row of data.models || []) {
    const label = row.subscription_requests
      ? "included in subscription"
      : row.priced_requests
        ? fmtMoney(row.cost.total, currency)
        : "unpriced";
    modelRows.append(el("tr", {}, [
      el("td", { class: "mono", text: row.provider }),
      el("td", { class: "mono", text: row.model }),
      el("td", { class: "mono", text: fmtCount(row.requests) }),
      el("td", { class: "mono", text: fmtCount(row.input_tokens + row.output_tokens) }),
      el("td", { class: "mono", text: label }),
    ]));
  }
  if (!modelRows.children.length) {
    modelRows.append(el("tr", {}, [el("td", { colspan: "5", text: "nothing recorded yet" })]));
  }

  renderInsight(data.insight, currency);
}

function renderInsight(insight, currency) {
  const host = $("#insight-list");
  host.replaceChildren();
  const suggestions = insight?.suggestions || [];
  $("#insight-empty").hidden = suggestions.length > 0;
  for (const suggestion of suggestions) {
    const apply = el("button", { class: "button button-primary", type: "button", text: "Apply routing change" });
    apply.addEventListener("click", async () => {
      const change = suggestion.apply;
      if (!confirm(`Point target ${change.target} at ${change.provider}/${change.model}?`)) return;
      const targets = structuredClone(configData?.integrated?.targets || {});
      const existing = targets[change.target];
      if (!existing) {
        toast(`Target ${change.target} no longer exists.`, "error");
        return;
      }
      existing.provider = change.provider;
      existing.model = change.model;
      try {
        const result = await apiSend("/api/integrated", "PUT", { targets });
        applyToast(result, `Target ${change.target}`);
        refreshFromResult(result);
        loadSpend();
      } catch (error) {
        toast("The routing change was not applied.", "error", error.message);
      }
    });
    host.append(el("div", { class: "insight-card" }, [
      el("p", { class: "insight-head mono", text: `${suggestion.target}: ${suggestion.from.provider}/${suggestion.from.model} → ${suggestion.to.provider}/${suggestion.to.model}` }),
      el("p", {
        class: "insight-body",
        text: `${fmtMoney(suggestion.from.cost_per_answer, currency)} per answer against ${fmtMoney(suggestion.to.cost_per_answer, currency)}, `
          + `on ${fmtCount(suggestion.from.answers)} and ${fmtCount(suggestion.to.answers)} observed answers. `
          + `Satisfaction ${fmtPercent(suggestion.from.satisfaction)} against ${fmtPercent(suggestion.to.satisfaction)}. `
          + `Observed saving ${fmtMoney(suggestion.observed_saving, currency)}.`,
      }),
      el("div", { class: "form-actions" }, [apply]),
    ]));
  }
}

function renderLeaderboard() {
  const data = leaderboardData;
  const tbody = $("#leaderboard-table tbody");
  tbody.replaceChildren();
  if (!data || data.available === false) {
    $("#leaderboard-intro").textContent = data?.reason || "outcomes are not being recorded";
    return;
  }
  $("#leaderboard-intro").textContent =
    `Ranked on observed aftermath — immediate re-asks and failures — not a public benchmark. `
    + `Rows with fewer than ${data.min_ranked_answers} answers are shown but not ranked.`;
  for (const row of data.models || []) {
    tbody.append(el("tr", { class: row.ranked ? "" : "row-muted" }, [
      el("td", { class: "mono", text: row.rank ?? "—" }),
      el("td", { class: "mono", text: row.provider }),
      el("td", { class: "mono", text: row.model }),
      el("td", { class: "mono", text: fmtCount(row.answers) }),
      el("td", { class: "mono", text: fmtCount(row.retried) }),
      el("td", { class: "mono", text: fmtCount(row.failed) }),
      el("td", { class: "mono", text: fmtPercent(row.satisfaction) }),
      el("td", { class: "mono", text: row.cost_per_answer === null || row.cost_per_answer === undefined ? "—" : fmtMoney(row.cost_per_answer, data.currency) }),
    ]));
  }
  if (!tbody.children.length) {
    tbody.append(el("tr", {}, [el("td", { colspan: "8", text: "nothing observed yet" })]));
  }

  const hints = $("#hint-candidates");
  hints.replaceChildren();
  for (const [target, entry] of Object.entries(data.hint_candidates || {})) {
    const terms = (entry.terms || []).map((term) => term.term).join(", ");
    hints.append(el("p", { class: "hint" }, [
      el("span", { class: "mono", text: `${target} ` }),
      el("span", { text: terms ? `(${entry.prompts} prompts): ${terms}` : `(${entry.prompts} prompts): nothing distinctive yet` }),
    ]));
  }
  if (!hints.children.length) {
    hints.append(el("p", { class: "hint", text: "no routed prompts observed in the last 24 hours" }));
  }
}

/* Shadow comparisons */

let shadowLastId = 0;

async function pollShadow() {
  try {
    const data = await apiGet(`/api/shadow?after=${shadowLastId}`);
    const host = $("#shadow-list");
    for (const entry of data.entries || []) {
      shadowLastId = Math.max(shadowLastId, entry.id);
      $("#shadow-empty").hidden = true;
      const card = el("div", { class: "shadow-card" }, [
        el("p", { class: "shadow-head mono" }, [
          el("span", { text: `${entry.primary.target} (${entry.primary.model})` }),
          el("span", { class: "shadow-vs", text: " vs " }),
          el("span", { text: `${entry.shadow.target} (${entry.shadow.model})` }),
          el("span", { class: "shadow-overlap", text: ` overlap ${fmtPercent(entry.overlap)}` }),
        ]),
        el("p", { class: "shadow-prompt", text: entry.prompt }),
        el("div", { class: "shadow-pair" }, [
          el("div", { class: "shadow-answer" }, [
            el("h4", { class: "sub-label mono", text: "primary — returned to the caller" }),
            el("pre", { class: "mono", text: entry.primary.answer || "(no text)" }),
          ]),
          el("div", { class: "shadow-answer" }, [
            el("h4", { class: "sub-label mono", text: "shadow — never seen by the caller" }),
            el("pre", { class: "mono", text: entry.shadow.error || entry.shadow.answer || "(no text)" }),
          ]),
        ]),
      ]);
      host.prepend(card);
    }
    while (host.querySelectorAll(".shadow-card").length > 50) {
      host.querySelector(".shadow-card:last-of-type")?.remove();
    }
  } catch {
    // The shadow feed is diagnostic; a poll failure is not worth a toast.
  }
}

/* Project memory */

let memoryData = null;

async function loadMemory() {
  try {
    const project = $("#memory-project").value;
    memoryData = await apiGet(`/api/memory${project ? `?project=${encodeURIComponent(project)}` : ""}`);
    renderMemory();
  } catch (error) {
    toast("Failed to load project memory.", "error", String(error.message || error));
  }
}

function renderMemory() {
  const data = memoryData;
  if (!data) return;
  fillSelect($("#memory-project"), data.projects || [], data.project, "current project");
  const status = $("#memory-status");
  if (!data.available) {
    status.textContent = "Project memory is not enabled. Turn it on under Settings.";
  } else if (!data.project) {
    status.textContent = "No project scope. Launch an agent from a directory containing a .llmproxy.toml, or pick a project above.";
  } else {
    status.textContent = `Shared across every agent that runs in ${data.project}.`;
  }
  const list = $("#memory-list");
  list.replaceChildren();
  const notes = [...(data.notes || [])].reverse();
  $("#memory-empty").hidden = notes.length > 0;
  for (const note of notes) {
    const remove = el("button", { class: "button button-danger", type: "button", text: "Forget" });
    remove.addEventListener("click", async () => {
      const project = data.project ? `?project=${encodeURIComponent(data.project)}` : "";
      try {
        await apiSend(`/api/memory/${note.id}${project}`, "DELETE");
        loadMemory();
      } catch (error) {
        toast("The note was not removed.", "error", error.message);
      }
    });
    list.append(el("li", { class: "memory-note" }, [
      el("span", { class: "memory-text", text: note.text }),
      note.source ? el("span", { class: "badge badge-neutral", text: note.source }) : null,
      remove,
    ]));
  }
}

/* Provider health */

async function loadHealth() {
  try {
    const data = await apiGet("/api/health");
    const tbody = $("#health-table tbody");
    tbody.replaceChildren();
    for (const row of data.providers || []) {
      tbody.append(el("tr", {}, [
        el("td", { class: "mono", text: row.provider }),
        el("td", {}, [el("span", { class: `badge badge-${row.status === "closed" ? "local" : "external"}`, text: row.status })]),
        el("td", { class: "mono", text: fmtCount(row.successes) }),
        el("td", { class: "mono", text: fmtCount(row.failures) }),
        el("td", { class: "mono", text: row.retry_in_secs === null || row.retry_in_secs === undefined ? "—" : `${row.retry_in_secs}s` }),
        el("td", { text: row.last_error || "" }),
      ]));
    }
    if (!tbody.children.length) {
      tbody.append(el("tr", {}, [el("td", { colspan: "6", text: data.enabled ? "every provider is healthy" : "health tracking is disabled" })]));
    }
  } catch {
    // Health is diagnostic; a poll failure is not worth a toast.
  }
}

/* Routing regression suite */

async function captureCorpus() {
  try {
    const mode = promptMode;
    const corpus = await apiGet(`/api/route-corpus?mode=${encodeURIComponent(mode)}`);
    $("#corpus-json").value = JSON.stringify(corpus, null, 2);
    updateCorpusCount();
    toast(`Captured ${corpus.cases.length} case(s) from the decision audit.`, "ok");
  } catch (error) {
    toast("The corpus was not captured.", "error", String(error.message || error));
  }
}

function updateCorpusCount() {
  const raw = $("#corpus-json").value.trim();
  if (!raw) {
    $("#corpus-count").textContent = "no corpus loaded";
    return;
  }
  try {
    const parsed = JSON.parse(raw);
    const cases = Array.isArray(parsed) ? parsed : parsed.cases || [];
    $("#corpus-count").textContent = `${cases.length} case(s) loaded`;
  } catch {
    $("#corpus-count").textContent = "corpus is not valid JSON";
  }
}

function downloadCorpus() {
  const raw = $("#corpus-json").value.trim();
  if (!raw) {
    toast("There is no corpus to download.", "error");
    return;
  }
  const url = URL.createObjectURL(new Blob([raw], { type: "application/json" }));
  const link = el("a", { href: url, download: "llmproxy-route-corpus.json" });
  document.body.append(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

async function replayCorpus() {
  const raw = $("#corpus-json").value.trim();
  if (!raw) {
    toast("Load or capture a corpus first.", "error");
    return;
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    toast("The corpus is not valid JSON.", "error", String(error.message || error));
    return;
  }
  const cases = Array.isArray(parsed) ? parsed : parsed.cases || [];
  const mode = (Array.isArray(parsed) ? null : parsed.mode) || promptMode;
  $("#corpus-running").hidden = false;
  $("#corpus-replay").disabled = true;
  try {
    const summary = await apiSend("/api/route-corpus/replay", "POST", {
      mode,
      instructions: $("#sys-editor").value,
      cases,
    });
    renderCorpusResults(summary);
  } catch (error) {
    toast("The replay did not finish.", "error", String(error.message || error));
  } finally {
    $("#corpus-running").hidden = true;
    $("#corpus-replay").disabled = false;
  }
}

function renderCorpusResults(summary) {
  const host = $("#corpus-results");
  host.replaceChildren();
  const headline = summary.regressions
    ? `${summary.regressions} regression(s) out of ${summary.cases} case(s)`
    : `no regressions across ${summary.cases} case(s)`;
  host.append(el("p", {
    class: `corpus-headline ${summary.regressions ? "is-external" : "is-local"}`,
    text: `${headline} — ${summary.same} same, ${summary.changed} changed, ${summary.new} new, ${summary.errors} errored`,
  }));
  const table = el("table", { class: "data-table" }, [
    el("thead", {}, [el("tr", {}, ["verdict", "was", "now", "prompt"].map((name) => el("th", { text: name })))]),
  ]);
  const tbody = el("tbody");
  for (const row of summary.results || []) {
    if (row.verdict === "same") continue;
    const label = (side) => [side.decision || "—", side.target].filter(Boolean).join(" → ");
    tbody.append(el("tr", {}, [
      el("td", {}, [el("span", { class: `badge badge-${row.verdict === "changed" || row.verdict === "error" ? "external" : "neutral"}`, text: row.verdict })]),
      el("td", { class: "mono", text: label(row.expected) }),
      el("td", { class: "mono", text: label(row.actual) }),
      el("td", { text: row.prompt }),
    ]));
  }
  if (!tbody.children.length) {
    tbody.append(el("tr", {}, [el("td", { colspan: "4", text: "every case matched what the corpus recorded" })]));
  }
  table.append(tbody);
  host.append(el("div", { class: "table-scroll bounded-table" }, [table]));
}

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

/// Provider name to its current breaker status, refreshed alongside stats so
/// an absorbed outage is visible here rather than only in the logs.
let providerHealth = {};

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
        healthBadge(provider.name),
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

function healthBadge(provider) {
  const status = providerHealth[provider];
  if (!status || status.status === "closed") return null;
  const label = status.status === "rate_limited" ? "rate limited" : status.status.replace("_", "-");
  const retry = status.retry_in_secs ? ` ${status.retry_in_secs}s` : "";
  return el("span", {
    class: "badge badge-external",
    text: `${label}${retry}`,
    title: status.last_error || "",
  });
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

/* Flow ribbons: one per provider that has served a call.

   Colour stays bound to locality so the strip matches the badges, stat tiles,
   and latency rows; identity is carried by stable position and a direct label,
   never by colour alone. Ribbon order must not follow request count, or
   providers swap places on every poll. Nodes are reused across polls because
   recreating them restarts the drift animation. */
const FLOW_MAX_RIBBONS = 6;
const FLOW_HUB_MID = 84;
const flowNodes = new Map();

function flowRibbons(providers) {
  const classRank = { local: 0, external: 1 };
  const entries = Object.entries(providers).map(([name, entry]) => ({
    key: name,
    count: entry.count || 0,
    cls: providerClasses[name] || "neutral",
    text: `${fmtCount(entry.count || 0)} ${name}`,
    title: entry.samples
      ? `${name} · ${fmtCount(entry.count || 0)} calls · ${fmtMs(entry.p50_ms)} → ${fmtMs(entry.p99_ms)}`
      : `${name} · ${fmtCount(entry.count || 0)} calls`,
  }));
  if (!entries.length) {
    return [{ key: "\u0000empty", count: 0, cls: "neutral", text: "no responses yet", title: "no responses yet" }];
  }
  // Position is an identity, not a ranking.
  const stable = (list) =>
    list.sort((a, b) => {
      const byClass = (classRank[a.cls] ?? 2) - (classRank[b.cls] ?? 2);
      return byClass !== 0 ? byClass : a.key.localeCompare(b.key);
    });
  if (entries.length <= FLOW_MAX_RIBBONS) return stable(entries);
  // Only fold membership is count-based, and it changes rarely.
  const byCount = [...entries].sort((a, b) => b.count - a.count);
  const folded = byCount.slice(FLOW_MAX_RIBBONS - 1);
  const foldedCount = folded.reduce((sum, entry) => sum + entry.count, 0);
  return [
    ...stable(byCount.slice(0, FLOW_MAX_RIBBONS - 1)),
    {
      key: "\u0000others",
      count: foldedCount,
      cls: "neutral",
      text: `${fmtCount(foldedCount)} +${folded.length} others`,
      title: folded.map((entry) => `${entry.key} · ${fmtCount(entry.count)}`).join("\n"),
    },
  ];
}

function renderFlowOut(providers, totalServed) {
  const group = $("#flow-out");
  const ribbons = flowRibbons(providers);
  const count = ribbons.length;
  // 100px of vertical room, but keep two ribbons as far apart as the old
  // hardcoded pair rather than bunching them in the middle.
  const spacing = count > 1 ? Math.min(44, 100 / (count - 1)) : 0;
  const fan = Math.min(10, spacing / 2.5);
  const strokeWidth = (n) => (totalServed ? 2 + 5 * (n / totalServed) : 2).toFixed(2);

  const seen = new Set();
  let membershipChanged = false;
  ribbons.forEach((ribbon, index) => {
    const offset = index - (count - 1) / 2;
    const endY = (FLOW_HUB_MID + offset * spacing).toFixed(1);
    const startY = (FLOW_HUB_MID + offset * fan).toFixed(1);
    let node = flowNodes.get(ribbon.key);
    if (!node) {
      const path = svgEl("path", { class: "flow-line drift" });
      const title = svgEl("title");
      path.append(title);
      const label = svgEl("text", { class: "flow-count mono", x: 584 });
      group.append(path, label);
      node = { path, label, title };
      flowNodes.set(ribbon.key, node);
      membershipChanged = true;
    }
    node.path.setAttribute("d", `M 388 ${startY} C 470 ${startY} 490 ${endY} 570 ${endY}`);
    node.path.setAttribute("class", `flow-line drift ${ribbon.cls}${ribbon.count ? "" : " idle"}`);
    node.path.style.strokeWidth = strokeWidth(ribbon.count);
    node.title.textContent = ribbon.title;
    node.label.setAttribute("class", `flow-count mono ${ribbon.cls}`);
    node.label.setAttribute("y", (Number(endY) + 5).toFixed(1));
    node.label.textContent = ribbon.text;
    seen.add(ribbon.key);
  });

  for (const [key, node] of flowNodes) {
    if (seen.has(key)) continue;
    node.path.remove();
    node.label.remove();
    flowNodes.delete(key);
    membershipChanged = true;
  }

  // Align DOM order with visual order so overlapping strokes stack
  // predictably. Only on membership change: moving a node restarts its drift
  // animation, which would stutter on every poll.
  if (membershipChanged) {
    for (const ribbon of ribbons) {
      const node = flowNodes.get(ribbon.key);
      group.append(node.path, node.label);
    }
  }
}

function renderStats(stats) {
  $("#meta-uptime").textContent = fmtUptime(stats.uptime_secs);

  const spend = stats.spend || {};
  if (spend.configured) {
    $("#stat-spend").textContent = fmtMoney(spend.total, spend.currency);
    const notes = [];
    if (spend.estimated_requests) notes.push(`${fmtCount(spend.estimated_requests)} estimated`);
    if (spend.subscription_requests) notes.push(`${fmtCount(spend.subscription_requests)} on a plan`);
    if (spend.unpriced_requests) notes.push(`${fmtCount(spend.unpriced_requests)} unpriced`);
    $("#stat-spend-sub").textContent = notes.length ? notes.join(" · ") : "since start";
  } else {
    $("#stat-spend").textContent = "—";
    $("#stat-spend-sub").textContent = "no prices configured";
  }

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
    (intercept.reject ? ` · reject ${fmtCount(intercept.reject)}` : "") +
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

  // Flow strip: one ribbon per provider, stroke weight following its share.
  const totalServed = served.local + served.external;
  $("#flow-in-count").textContent = fmtCount(requests.total);
  renderFlowOut(served.providers || {}, totalServed);
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
    const [stats, health, cache] = await Promise.all([
      apiGet("/api/stats"),
      apiGet("/api/health"),
      apiGet("/api/route-cache"),
    ]);
    providerHealth = Object.fromEntries(
      (health.providers || []).map((row) => [row.provider, row])
    );
    renderStats(stats);
    renderRouteCacheStats(cache);
    $("#lamp-proxy").className = "lamp on";
  } catch {
    $("#lamp-proxy").className = "lamp off";
  }
}

function renderRouteCacheStats(cache) {
  const node = $("#route-cache-stats");
  if (!node) return;
  node.textContent = cache.lookups
    ? `cache: ${fmtCount(cache.entries)} entries · ${fmtPercent(cache.hit_rate)} of ${fmtCount(cache.lookups)} lookups hit`
    : `cache: ${fmtCount(cache.entries)} entries · no lookups yet`;
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

/* Routing decision audit */

let decisionLastId = 0;
let decisionEntries = [];
const DECISION_BOARD_MAX = 512;

function decisionGroup(entry) {
  if (entry.decision === "reject") return "reject";
  if (entry.error || ["error", "fallback", "invalid"].includes(entry.decision)) return "error";
  if (entry.decision === "bypass") return "bypass";
  return entry.class || (entry.decision === "local" ? "local" : "external");
}

function destinationLabel(entry) {
  if (entry.decision === "reject") return "no external call";
  if (!entry.provider && !entry.model) return "destination unresolved";
  return [entry.provider || "unknown", entry.model || "unknown"].join(" / ");
}

function decisionMatches(entry) {
  const outcome = $("#decision-outcome").value;
  if (outcome && decisionGroup(entry) !== outcome) return false;
  const query = $("#decision-filter").value.trim().toLocaleLowerCase();
  if (!query) return true;
  return [
    entry.prompt,
    entry.reason,
    entry.provider,
    entry.model,
    entry.target,
    entry.target_hint,
    entry.requested_model,
    entry.request_id,
  ]
    .filter(Boolean)
    .join(" ")
    .toLocaleLowerCase()
    .includes(query);
}

function useDecisionInTest(entry) {
  $("#test-prompt").value = entry.prompt || "";
  setPromptMode(entry.mode === "integrated" ? "integrated" : "intercept");
  location.hash = "#/routing/policy";
  $("#test-prompt").focus();
}

function renderDecisions() {
  const ledger = $("#decision-ledger");
  for (const card of ledger.querySelectorAll(".decision-card")) card.remove();
  const visible = decisionEntries.filter(decisionMatches).slice().reverse();
  $("#decision-empty").hidden = visible.length > 0;
  $("#decision-empty").textContent = decisionEntries.length
    ? "no decisions match these filters"
    : "no routing decisions yet";
  for (const entry of visible) {
    const group = decisionGroup(entry);
    const time = new Date(entry.ts_ms).toLocaleString("en-GB");
    const requested = entry.requested_model || "unspecified";
    const triage = entry.triage_model
      ? `${entry.triage_provider || "unknown"} / ${entry.triage_model}`
      : "not run";
    const routeMeta = [
      `${entry.mode} · ${entry.protocol}`,
      `requested ${requested}`,
      `triage ${triage}`,
      entry.triage_elapsed_ms !== null && entry.triage_elapsed_ms !== undefined
        ? fmtMs(entry.triage_elapsed_ms)
        : null,
      `${entry.policy} policy`,
    ]
      .filter(Boolean)
      .join(" · ");
    const destinationMeta = [
      destinationLabel(entry),
      entry.class,
      entry.reasoning_effort && `${entry.reasoning_effort} reasoning`,
      entry.target && `target ${entry.target}`,
    ]
      .filter(Boolean)
      .join(" · ");
    ledger.append(
      el("article", { class: `decision-card decision-${group}` }, [
        el("div", { class: "decision-card-top" }, [
          el("span", { class: `decision-badge decision-badge-${group} mono`, text: entry.decision }),
          el("span", { class: "decision-destination mono", text: destinationMeta }),
          el("time", { class: "decision-time mono", datetime: new Date(entry.ts_ms).toISOString(), text: time }),
        ]),
        el("p", { class: "decision-reason", text: entry.reason || "No audit reason was supplied." }),
        entry.target_hint
          ? el("p", { class: "decision-hint", text: `route hint: ${entry.target_hint}` })
          : null,
        el("blockquote", { class: "decision-prompt", text: entry.prompt || "(no user text)" }),
        entry.error ? el("p", { class: "decision-error-text mono", text: entry.error }) : null,
        el("div", { class: "decision-card-foot" }, [
          el("span", { class: "decision-meta mono", text: routeMeta }),
          entry.decision === "reject"
            ? el("span", { class: "decision-redacted mono", text: "prompt redacted" })
            : el("button", {
                class: "button button-quiet decision-test",
                type: "button",
                text: "Test this prompt",
                onclick: () => useDecisionInTest(entry),
              }),
        ]),
      ])
    );
  }
}

async function pollDecisions() {
  try {
    const data = await apiGet(`/api/decisions?after=${decisionLastId}`);
    decisionLastId = data.latest_id ?? decisionLastId;
    if (data.entries?.length) {
      decisionEntries.push(...data.entries);
      decisionEntries = decisionEntries.slice(-DECISION_BOARD_MAX);
      renderDecisions();
    }
  } catch {
    /* The proxy lamp reflects API connectivity; older builds may not expose audits. */
  }
}

function startPolling() {
  if (statsTimer) clearInterval(statsTimer);
  statsTimer = setInterval(() => {
    if (!document.hidden) {
      pollStats();
      pollLogs();
      pollErrors();
      pollDecisions();
      // The shadow feed only polls while it is on screen: each entry carries
      // two whole answers, so it is far heavier than the other feeds.
      if (currentRoute().secondary === "shadow") pollShadow();
    }
  }, 2000);
  pollStats();
  pollLogs();
  pollErrors();
  pollDecisions();
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
      : overview.interceptor.reject_sensitive
        ? `sensitive screening on · ${overview.interceptor.model}`
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
    case "reject":
      return "→ rejected (no action)";
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
    verdict.reason
      ? el("div", { class: "test-reason", text: `audit reason: ${verdict.reason}` })
      : null,
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
  $("#int-reject-sensitive").checked = interceptor.reject_sensitive;
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
  // Exactly one target may be the catch-all, so the checkboxes behave as a
  // radio group without pretending to be one in the markup.
  const isDefault = el("input", {
    type: "checkbox", class: "target-default", "aria-label": `Catch-all route for ${name}`,
    title: "ROUTE_DEFAULT resolves here instead of falling through to model discovery",
  });
  isDefault.checked = Boolean(target.default);
  isDefault.addEventListener("change", () => {
    if (!isDefault.checked) return;
    for (const other of document.querySelectorAll(".target-default")) {
      if (other !== isDefault) other.checked = false;
    }
  });
  const strength = el("input", {
    class: "input mono target-strength", type: "number", min: "0", step: "1",
    value: target.strength ?? "", placeholder: "—",
    "aria-label": `Routing strength for ${name}`,
    title: "Higher is stronger. Only the ordering matters, and only to the escalate policy.",
  });
  const failover = el("input", {
    class: "input mono target-failover", value: (target.failover || []).join(", "),
    placeholder: "—", "aria-label": `Failover chain for ${name}`,
    title: "Comma-separated target names tried when this one is unavailable or fails",
  });
  const remove = el("button", {
    class: "button button-danger",
    type: "button",
    text: "Remove",
    onclick: (event) => event.target.closest("tr").remove(),
  });
  const row = el("tr", {}, [
    el("td", {}, [targetName]),
    el("td", {}, [provider]),
    el("td", {}, [model]),
    el("td", {}, [effort]),
    el("td", { class: "cell-check" }, [isDefault]),
    el("td", {}, [strength]),
    el("td", {}, [failover]),
    el("td", {}, [hint]),
    el("td", {}, [remove]),
  ]);
  // Second opinions are rare and structural, so they ride along on the row
  // rather than occupying a column every target would leave empty.
  row.dataset.secondOpinion = target.second_opinion ? JSON.stringify(target.second_opinion) : "";
  tbody.append(row);
}

function targetOptions(select, values, selected, emptyLabel) {
  select.replaceChildren(el("option", { value: "", text: emptyLabel }));
  for (const name of values) select.append(el("option", { value: name, text: name }));
  select.value = values.includes(selected) ? selected : "";
}

function renderIntegratedForm() {
  const integrated = configData.integrated;
  $("#integrated-enabled").checked = integrated.enabled;
  $("#integrated-ttl").value = integrated.sticky_ttl_secs;
  $("#integrated-abs-ttl").value = integrated.sticky_absolute_ttl_secs;
  $("#integrated-max-turns").value = integrated.sticky_max_turns;
  $("#integrated-policy").value = integrated.sticky_policy;
  $("#integrated-capacity").value = integrated.sticky_capacity;
  $("#integrated-verbs").checked = integrated.verbs;
  $("#integrated-verb-list").textContent =
    `${(integrated.verb_vocabulary || []).map((verb) => `${verb}:`).join(" ")} and any target name`;
  $("#integrated-speculative").checked = integrated.speculative;
  $("#integrated-failover").value = (integrated.failover || []).join(", ");

  const shadow = integrated.shadow || {};
  $("#shadow-enabled").checked = Boolean(shadow.enabled);
  $("#shadow-sample").value = shadow.sample_one_in ?? 20;
  $("#shadow-timeout").value = shadow.timeout_ms ?? 120000;

  const embedding = integrated.embedding || {};
  $("#embed-enabled").checked = Boolean(embedding.enabled);
  $("#embed-model").value = embedding.model || "";
  $("#embed-threshold").value = embedding.threshold ?? 0.94;
  $("#embed-retriage").value = embedding.retriage_below ?? 0;
  $("#embed-retry-threshold").value = embedding.retry_threshold ?? 0.93;
  $("#embed-capacity").value = embedding.capacity ?? 2048;
  $("#embed-timeout").value = embedding.timeout_ms ?? 2000;
  $("#embed-retry").checked = Boolean(embedding.retry_escalation);
  providerOptions($("#embed-provider"), embedding.provider || "", false);

  const tbody = $("#integrated-targets tbody");
  tbody.replaceChildren();
  const names = Object.keys(integrated.targets).sort((a, b) => a.localeCompare(b));
  for (const name of names) appendIntegratedTargetRow(name, integrated.targets[name]);
  if (!tbody.children.length) {
    tbody.append(el("tr", { class: "integrated-empty" }, [el("td", { colspan: "9", text: "no redirect targets configured" })]));
  }
  targetOptions($("#shadow-target"), names, shadow.target || "", "pick a target");
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
    const strengthValue = row.querySelector(".target-strength").value.trim();
    const failover = row.querySelector(".target-failover").value
      .split(",")
      .map((entry) => entry.trim())
      .filter(Boolean);
    let secondOpinion = null;
    if (row.dataset.secondOpinion) {
      try {
        secondOpinion = JSON.parse(row.dataset.secondOpinion);
      } catch {
        secondOpinion = null;
      }
    }
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(name) || name === "PASS" || name === "ROUTE_DEFAULT") {
      throw new Error(`${name || "target"}: use a non-reserved 1-64 character tool name containing letters, digits, _, or -`);
    }
    if (!provider || !model || !hint || hint.includes("\n")) throw new Error(`${name}: provider, model, and one-line hint are required`);
    if (targets[name]) throw new Error(`duplicate integrated target ${name}`);
    targets[name] = {
      provider,
      model,
      reasoning_effort: reasoningEffort || null,
      hint,
      default: row.querySelector(".target-default").checked,
      strength: strengthValue === "" ? null : Number(strengthValue),
      failover,
      second_opinion: secondOpinion,
    };
  }
  if (Object.values(targets).filter((target) => target.default).length > 1) {
    throw new Error("only one target may be the catch-all route");
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

    // Rates are per million tokens so a config entry can be copied straight
    // off a vendor price page. Nothing is inferred: a provider with no table
    // simply reads as unpriced everywhere.
    const pricing = provider.pricing || { subscription: false, currency: "USD", models: {} };
    const subscription = el("input", { type: "checkbox", "aria-label": `Subscription billing for ${name}` });
    subscription.checked = Boolean(pricing.subscription);
    const currency = el("input", { class: "input mono", value: pricing.currency || "USD", "aria-label": `Currency for ${name}` });
    const priceRows = el("tbody");
    const addPriceRow = (model, price) => {
      priceRows.append(el("tr", {}, [
        el("td", {}, [el("input", { class: "input mono price-model", value: model, placeholder: "gpt-5.4 or *", "aria-label": "Priced model" })]),
        el("td", {}, [el("input", { class: "input mono price-input", type: "number", min: "0", step: "0.01", value: price.input ?? 0 })]),
        el("td", {}, [el("input", { class: "input mono price-output", type: "number", min: "0", step: "0.01", value: price.output ?? 0 })]),
        el("td", {}, [el("input", { class: "input mono price-cache-read", type: "number", min: "0", step: "0.01", value: price.cache_read ?? "", placeholder: "input rate" })]),
        el("td", {}, [el("input", { class: "input mono price-cache-write", type: "number", min: "0", step: "0.01", value: price.cache_write ?? "", placeholder: "input rate" })]),
        el("td", {}, [el("button", {
          class: "button button-danger", type: "button", text: "Remove",
          onclick: (event) => event.target.closest("tr").remove(),
        })]),
      ]));
    };
    for (const [model, price] of Object.entries(pricing.models || {}).sort(([a], [b]) => a.localeCompare(b))) {
      addPriceRow(model, price);
    }
    const addPrice = el("button", {
      class: "button", type: "button", text: "Add model rate",
      onclick: () => addPriceRow("", { input: 0, output: 0 }),
    });
    const collectPricing = () => {
      const models = {};
      for (const row of priceRows.querySelectorAll("tr")) {
        const model = row.querySelector(".price-model").value.trim();
        if (!model) continue;
        const rate = (selector) => {
          const raw = row.querySelector(selector).value.trim();
          return raw === "" ? null : Number(raw);
        };
        models[model] = {
          input: Number(row.querySelector(".price-input").value) || 0,
          output: Number(row.querySelector(".price-output").value) || 0,
          cache_read: rate(".price-cache-read"),
          cache_write: rate(".price-cache-write"),
        };
      }
      return { subscription: subscription.checked, currency: currency.value.trim() || "USD", models };
    };
    const pricingBlock = el("details", { class: "adv" }, [
      el("summary", { text: "Pricing" }),
      el("p", { class: "hint", text: "Rates are per million tokens. A subscription provider reports token volume but never a dollar figure, because it bills against a plan." }),
      el("div", { class: "field-row" }, [
        el("label", { class: "field field-check" }, [subscription, el("span", { text: "Billed against a plan, not per token" })]),
        el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "currency" }), currency]),
      ]),
      el("div", { class: "table-scroll bounded-table" }, [
        el("table", { class: "data-table edit-table" }, [
          el("thead", {}, [el("tr", {}, ["model", "input", "output", "cache read", "cache write", ""].map((label) => el("th", { text: label })))]),
          priceRows,
        ]),
      ]),
      el("div", { class: "form-actions" }, [addPrice]),
    ]);

    const save = el("button", { class: "button button-primary", type: "button", text: "Save" });
    save.addEventListener("click", async () => {
      const body = {
        kind: kind.value,
        base_url: baseUrl.value.trim(),
        reasoning_effort: reasoningEffort.value,
        timeout_secs: Number(timeout.value) || provider.timeout_secs,
        connect_timeout_secs: Number(connectTimeout.value) || provider.connect_timeout_secs,
        pricing: collectPricing(),
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
      provider.pricing?.subscription
        ? el("span", { class: "badge badge-neutral", text: "subscription" })
        : provider.pricing?.configured
          ? el("span", { class: "badge badge-neutral", text: "priced" })
          : null,
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
      pricingBlock,
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

  const health = configData.health || {};
  $("#set-health-enabled").checked = Boolean(health.enabled);
  $("#set-health-threshold").value = health.failure_threshold ?? 4;
  $("#set-health-open").value = health.open_secs ?? 30;
  $("#set-health-max-open").value = health.max_open_secs ?? 600;

  const memory = configData.memory || {};
  $("#set-memory-enabled").checked = Boolean(memory.enabled);
  $("#set-memory-inject").checked = Boolean(memory.inject);
  $("#set-memory-notes").value = memory.max_notes ?? 64;
  $("#set-memory-chars").value = memory.max_injected_chars ?? 6000;

  const redact = configData.interceptor.redact || {};
  $("#set-redact-enabled").checked = Boolean(redact.enabled);
  $("#set-redact-rehydrate").checked = Boolean(redact.rehydrate);
  $("#set-redact-literals").value = (redact.extra_literals || []).join(", ");
  $("#set-triage-max").value = configData.interceptor.triage_max_messages ?? 0;

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

  $("#decision-filter").addEventListener("input", renderDecisions);
  $("#decision-outcome").addEventListener("change", renderDecisions);
  $("#clear-decisions").addEventListener("click", async () => {
    try {
      await apiSend("/api/decisions/clear", "POST");
      decisionEntries = [];
      renderDecisions();
      toast("Routing decision audit cleared.");
    } catch (error) {
      toast("Could not clear routing decisions.", "error", error.message);
    }
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
      reject_sensitive: $("#int-reject-sensitive").checked,
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
        sticky_absolute_ttl_secs: Number($("#integrated-abs-ttl").value),
        sticky_max_turns: Number($("#integrated-max-turns").value),
        sticky_policy: $("#integrated-policy").value,
        sticky_capacity: Number($("#integrated-capacity").value),
        verbs: $("#integrated-verbs").checked,
        speculative: $("#integrated-speculative").checked,
        failover: $("#integrated-failover").value.split(",").map((entry) => entry.trim()).filter(Boolean),
        shadow: {
          enabled: $("#shadow-enabled").checked,
          target: $("#shadow-target").value,
          sample_one_in: Math.max(1, Number($("#shadow-sample").value) || 20),
          timeout_ms: Math.max(1, Number($("#shadow-timeout").value) || 120000),
        },
        embedding: {
          enabled: $("#embed-enabled").checked,
          provider: $("#embed-provider").value,
          model: $("#embed-model").value.trim(),
          threshold: Number($("#embed-threshold").value),
          capacity: Math.max(1, Number($("#embed-capacity").value) || 2048),
          ttl_secs: configData?.integrated?.embedding?.ttl_secs ?? 86400,
          retriage_below: Number($("#embed-retriage").value),
          retry_escalation: $("#embed-retry").checked,
          retry_threshold: Number($("#embed-retry-threshold").value),
          timeout_ms: Math.max(1, Number($("#embed-timeout").value) || 2000),
        },
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
        health: {
          enabled: $("#set-health-enabled").checked,
          failure_threshold: Math.max(1, Number($("#set-health-threshold").value) || 4),
          open_secs: Math.max(1, Number($("#set-health-open").value) || 30),
          max_open_secs: Math.max(1, Number($("#set-health-max-open").value) || 600),
          max_rate_limit_secs: configData?.health?.max_rate_limit_secs ?? 900,
        },
        memory: {
          enabled: $("#set-memory-enabled").checked,
          inject: $("#set-memory-inject").checked,
          max_notes: Math.max(1, Number($("#set-memory-notes").value) || 64),
          max_note_chars: configData?.memory?.max_note_chars ?? 2000,
          max_injected_chars: Math.max(1, Number($("#set-memory-chars").value) || 6000),
        },
      });
      await apiSend("/api/interceptor", "PUT", {
        triage_max_messages: Math.max(0, Number($("#set-triage-max").value) || 0),
        redact: {
          enabled: $("#set-redact-enabled").checked,
          rehydrate: $("#set-redact-rehydrate").checked,
          extra_literals: $("#set-redact-literals").value
            .split(",")
            .map((entry) => entry.trim())
            .filter(Boolean),
        },
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

  $("#spend-refresh").addEventListener("click", loadSpend);
  $("#spend-month").addEventListener("change", loadSpend);
  $("#spend-project").addEventListener("change", loadSpend);
  $("#spend-reset").addEventListener("click", async () => {
    if (!confirm("Clear the durable usage ledger for every project and month?")) return;
    try {
      await apiSend("/api/spend/reset", "POST", {});
      toast("Usage ledger cleared.", "ok");
      loadSpend();
    } catch (error) {
      toast("The ledger was not cleared.", "error", error.message);
    }
  });

  $("#clear-shadow").addEventListener("click", async () => {
    try {
      await apiSend("/api/shadow/clear", "POST", {});
      shadowLastId = 0;
      $("#shadow-list").replaceChildren($("#shadow-empty"));
      $("#shadow-empty").hidden = false;
    } catch (error) {
      toast("Comparisons were not cleared.", "error", error.message);
    }
  });

  $("#memory-refresh").addEventListener("click", loadMemory);
  $("#memory-project").addEventListener("change", loadMemory);
  $("#memory-add-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const text = $("#memory-add-text").value.trim();
    if (!text) return;
    try {
      await apiSend("/api/memory", "POST", {
        project: $("#memory-project").value || undefined,
        text,
      });
      $("#memory-add-text").value = "";
      loadMemory();
    } catch (error) {
      toast("The note was not recorded.", "error", error.message);
    }
  });

  $("#health-reset").addEventListener("click", async () => {
    try {
      await apiSend("/api/health/reset", "POST", {});
      loadHealth();
      toast("Provider health cleared.", "ok");
    } catch (error) {
      toast("Health state was not cleared.", "error", error.message);
    }
  });

  $("#corpus-capture").addEventListener("click", captureCorpus);
  $("#corpus-download").addEventListener("click", downloadCorpus);
  $("#corpus-replay").addEventListener("click", replayCorpus);
  $("#corpus-json").addEventListener("input", updateCorpusCount);

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
