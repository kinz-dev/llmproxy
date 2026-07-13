/* llmproxy admin UI. No dependencies, no build step. */
"use strict";

const $ = (sel) => document.querySelector(sel);

/* ------------------------------------------------------------ helpers */

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

/* Result of a config-editing call: toast applied/persisted state. */
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

/* -------------------------------------------------------------- tabs */

function currentTab() {
  if (location.hash === "#/configure") return "configure";
  if (location.hash === "#/prompt") return "prompt";
  return "monitor";
}

function renderTab() {
  const tab = currentTab();
  for (const name of ["monitor", "configure", "prompt"]) {
    $(`#view-${name}`).hidden = tab !== name;
    $(`#tab-${name}`).setAttribute("aria-selected", String(tab === name));
  }
  if (tab === "configure") loadConfigure();
  if (tab === "prompt") loadPromptTab();
}

window.addEventListener("hashchange", renderTab);

/* ------------------------------------------------------------ monitor */

let launchData = null;

function launchSelection() {
  const agent = document.querySelector('input[name="agent"]:checked')?.value || "codex";
  const style = document.querySelector('input[name="style"]:checked')?.value || "launcher";
  return { agent, style };
}

function renderLaunch() {
  if (!launchData) return;
  const { agent, style } = launchSelection();
  const key = `${agent}_${style === "launcher" ? "launcher" : "env"}`;
  $("#launch-command").textContent = launchData.commands[key] || "—";

  const hints = [];
  if (style === "launcher") {
    const agentLabel = agent === "codex" ? "Codex" : agent === "claude" ? "Claude Code" : "Copilot CLI";
    hints.push(
      `Starts the proxy and ${agentLabel} together; ` +
      "the agent keeps its own login and the proxy daemon stays running."
    );
    hints.push(
      launchData.interceptor_configured
        ? `--intercept routes deterministic tasks to ${launchData.interceptor_model} for this session.`
        : "Configure [interceptor] provider and model first — the launcher requires them."
    );
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

function renderRouting(data) {
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
  $("#stat-remapped-sub").textContent = `alias ${fmtCount(requests.remap_alias)} · auto ${fmtCount(requests.remap_auto)}`;
  $("#stat-intercepted").textContent = fmtCount(intercept.attempts);
  $("#stat-intercepted-sub").textContent =
    `pass ${fmtCount(intercept.pass)} · fallback ${fmtCount(intercept.fallback)}` +
    (intercept.bypass ? ` · bypass ${fmtCount(intercept.bypass)}` : "") +
    (intercept.errors ? ` · errors ${fmtCount(intercept.errors)}` : "");
  $("#stat-local").textContent = fmtCount(served.local);
  $("#stat-local-sub").textContent = intercept.local ? `${fmtCount(intercept.local)} by interceptor` : " ";
  $("#stat-external").textContent = fmtCount(served.external);
  $("#stat-tokens").textContent = "≈ " + fmtCount(tokens.total);
  $("#stat-tokens-sub").textContent = `prompt ${fmtCount(tokens.prompt)} · completion ${fmtCount(tokens.completion)}`;

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

let statsTimer = null;

async function pollStats() {
  try {
    renderStats(await apiGet("/api/stats"));
    $("#lamp-proxy").className = "lamp on";
  } catch {
    $("#lamp-proxy").className = "lamp off";
  }
}

/* -------------------------------------------------------- traffic log */

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

/* Drop entries past the age limit and beyond the max-lines slider. */
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
        text: `[${entry.class}-${entry.dir}] ${entry.text}`,
      }),
    ]);
    node.dataset.ts = entry.ts_ms;
    board.append(node);
  }
  trimLogEntries(board);
  if (stick && entries.length) board.scrollTop = board.scrollHeight;
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
    /* otherwise the proxy lamp already reflects connectivity */
  }
}

function startPolling() {
  if (statsTimer) clearInterval(statsTimer);
  statsTimer = setInterval(() => {
    if (!document.hidden) {
      pollStats();
      pollLogs();
    }
  }, 2000);
  pollStats();
  pollLogs();
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

    $("#meta-proxy-addr").textContent = overview.proxy_addr || overview.listen_addr;
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

/* -------------------------------------------------- system prompt tab */

let sysEditorLoaded = false;
let sysUndoText = null;
const MAX_TEST_RESULTS = 5;

function fillSysEditor() {
  const editor = $("#sys-editor");
  editor.value = configData.interceptor.instructions;
  editor.dataset.saved = editor.value;
  sysUndoText = null;
  $("#revise-undo").hidden = true;
  updateSysStatus();
  updateSysDirty();
}

function updateSysStatus() {
  const interceptor = configData.interceptor;
  $("#sys-status").textContent =
    `${interceptor.provider} · ${interceptor.model} · interceptor ${interceptor.enabled ? "on" : "off"}` +
    (interceptor.enabled ? "" : " — tests below still run against the local model");
}

function updateSysDirty() {
  const editor = $("#sys-editor");
  if (editor.value !== editor.dataset.saved) {
    $("#sys-dirty").textContent = "unsaved changes — route tests use this text; press Save to persist";
  } else if (configData?.interceptor?.instructions_are_default) {
    $("#sys-dirty").textContent = "built-in default policy";
  } else {
    $("#sys-dirty").textContent = "saved custom policy";
  }
}

async function loadPromptTab() {
  try {
    if (!configData) configData = await apiGet("/api/config");
    if (!sysEditorLoaded) {
      fillSysEditor();
      sysEditorLoaded = true;
    } else {
      // Keep any unsaved editor text on tab revisits.
      updateSysStatus();
      updateSysDirty();
    }
  } catch (error) {
    toast("Failed to load the system prompt.", "error", String(error.message || error));
  }
}

async function saveSystemPrompt(text, what) {
  try {
    const result = await apiSend("/api/interceptor", "PUT", { instructions: text });
    applyToast(result, what);
    configData = result.config;
    fillSysEditor();
    renderConfigure();
  } catch (error) {
    toast(`${what} was not saved.`, "error", error.message);
  }
}

function verdictLabel(decision) {
  return { local: "→ local", external: "→ external", invalid: "invalid output", error: "error" }[decision] || decision;
}

function renderTestResult(prompt, verdict) {
  const host = $("#test-results");
  const meta = [
    verdict.model && `${verdict.provider} / ${verdict.model}`,
    verdict.elapsed_ms !== undefined && `${verdict.elapsed_ms} ms`,
    `“${prompt.length > 90 ? prompt.slice(0, 90) + "…" : prompt}”`,
  ]
    .filter(Boolean)
    .join(" · ");
  const card = el("div", { class: `test-result verdict-${verdict.decision}` }, [
    el("div", { class: "test-verdict mono", text: verdictLabel(verdict.decision) }),
    verdict.note ? el("div", { class: "test-note", text: verdict.note }) : null,
    el("div", { class: "test-meta mono", text: meta }),
    verdict.detail ? el("div", { class: "test-detail", text: verdict.detail }) : null,
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
    });
    renderTestResult(prompt, verdict);
  } catch (error) {
    toast("Route test failed.", "error", error.message);
  } finally {
    button.disabled = false;
    $("#test-running").hidden = true;
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

/* ---------------------------------------------------------- configure */

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
  const instructions = $("#int-instructions");
  instructions.value = interceptor.instructions_are_default ? "" : interceptor.instructions;
  instructions.dataset.initial = instructions.value;
}

function renderProviderEditors() {
  const host = $("#provider-editors");
  host.replaceChildren();
  const providers = Object.entries(configData.providers).sort(([a], [b]) => a.localeCompare(b));
  for (const [name, provider] of providers) {
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
        base_url: baseUrl.value.trim(),
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

    const head = el("div", { class: "provider-editor-head" }, [
      el("span", { class: "provider-name mono", text: name }),
      el("span", { class: `badge badge-${provider.class}`, text: provider.class }),
      provider.is_default ? el("span", { class: "badge badge-neutral", text: "default" }) : null,
      provider.from_upstream_section ? el("span", { class: "badge badge-neutral", text: "[upstream]" }) : null,
      el("span", { class: "spacer" }),
    ]);

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
      head.append(clearKey);
    }
    if (!provider.is_default && !provider.from_upstream_section) {
      const remove = el("button", { class: "button button-danger", type: "button", text: "Delete" });
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
      head.append(remove);
    }

    host.append(
      el("div", { class: "provider-editor" }, [
        head,
        el("div", { class: "field-row" }, [
          el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "base_url" }), baseUrl]),
        ]),
        el("div", { class: "field-row" }, [
          el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "api_key_env" }), apiKeyEnv]),
          el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "api_key" }), apiKey]),
        ]),
        el("div", { class: "field-row" }, [
          el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "timeout_secs" }), timeout]),
          el("label", { class: "field" }, [el("span", { class: "field-name mono", text: "connect_timeout_secs" }), connectTimeout]),
          el("div", { class: "field", style: "justify-content:flex-end" }, [save]),
        ]),
      ])
    );
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
  $("#set-responses-mode").value = configData.responses.mode;
  $("#set-disable-thinking").checked = configData.chat.disable_thinking;
  $("#set-expose-aliases").checked = configData.models.expose_aliases;
  $("#set-expose-upstream").checked = configData.models.expose_upstream;
  renderRoleMapRows(configData.role_map);
}

function renderConfigure() {
  if (!configData) return;
  renderAliasEditor();
  renderInterceptorForm();
  renderProviderEditors();
  renderSettingsForm();
}

function refreshFromResult(result) {
  if (result?.config) {
    configData = result.config;
    renderConfigure();
  }
  loadMonitor();
}

let configureRendered = false;

async function loadConfigure(force = false) {
  if (configureRendered && !force) return;
  try {
    if (!configData || force) configData = await apiGet("/api/config");
    renderConfigure();
    configureRendered = true;
    await loadRawConfig();
  } catch (error) {
    toast("Failed to load the configuration.", "error", String(error.message || error));
  }
}

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
      return;
    }
    editor.value = raw.toml;
    editor.disabled = false;
    $("#raw-apply").disabled = false;
    $("#raw-reload").disabled = false;
  } catch (error) {
    toast("Failed to read the config file.", "error", error.message);
  }
}

/* --------------------------------------------------------- wire it up */

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
    const instructions = $("#int-instructions");
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
    if (instructions.value !== instructions.dataset.initial) {
      body.instructions = instructions.value;
    }
    try {
      const result = await apiSend("/api/interceptor", "PUT", body);
      applyToast(result, "Interceptor");
      refreshFromResult(result);
    } catch (error) {
      toast("Interceptor settings were not saved.", "error", error.message);
    }
  });

  $("#provider-add-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const name = $("#provider-add-name").value.trim();
    try {
      const result = await apiSend(`/api/providers/${encodeURIComponent(name)}`, "PUT", {
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

  $("#sys-editor").addEventListener("input", updateSysDirty);
  $("#sys-save").addEventListener("click", () => saveSystemPrompt($("#sys-editor").value, "System prompt"));
  $("#sys-reset").addEventListener("click", () => {
    if (confirm("Replace the current policy with the built-in default?")) {
      saveSystemPrompt("", "System prompt reset;");
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
