# llmproxy Agent Instructions

## Start Here

1. Run `git status --short` and inspect the actual diff. The worktree may have
   concurrent user changes.
2. Inspect the live wrapper, effective config, listener, and process when
   diagnosing runtime behavior. Editable config is not automatically live.
3. Treat this file as the durable architecture and validation reference.

## Worktree Safety

- Never revert, overwrite, stage, or commit changes you did not make.
- Before edits, create an index-isolated checkpoint:

  ```sh
  git commit --allow-empty --only -m "checkpoint: before <task>"
  ```

- Stage explicit paths only. Preserve unrelated worktree and index state.
- `config.toml` is ignored and may contain credentials. Never print its secrets,
  force-add it, or replace it with the example config.
- Do not restart or replace the active proxy unless the user explicitly asks.
  Builds and isolated smoke tests are not deployment approval.

## Product Contract

`llmproxy` is a local-first Rust proxy for OpenAI Chat Completions and
Responses, Anthropic Messages, and native agent launcher traffic.

```text
agent request -> separate local interceptor request -> local response
                                               \-> PASS -> original external request (intercept)
                                               \-> Auto + ROUTE_DEFAULT -> normal configured routing
                                               \-> Auto + configured route -> provider/model
```

- The triage model receives a separate dynamic request with compatible tools.
  Model `Auto` enables integrated `ROUTE_DEFAULT` and configured route tools.
  Other models bypass integrated triage; they receive `PASS` only when the
  standalone interceptor is enabled. The local copy must never mutate the
  external copy.
- Routing controls are valid only as the sole tool call or exact text output.
  Mixed output is invalid. Integrated mode accepts legacy `PASS` output but
  does not advertise it. Tool calls may carry only a concise `reason` audit
  field; legacy empty arguments remain valid, and audit metadata is never
  rendered to the caller.
- Optional `interceptor.reject_sensitive` adds a mandatory, dynamically
  appended `REJECT` control before local/external routing. Its reason is a safe
  category, never the detected value. Rejection renders a native successful
  no-action response, disables sticky bypass, and fails closed when screening
  cannot complete. Screened request/triage bodies are omitted from TRACE, and
  rejected audit prompts are redacted. Pure `--proxy` launcher sessions
  disable it to remain prompt-free.
- Caller authentication, account headers, and external credentials must never
  reach the local provider.
- Accepted local output is rendered in the caller's native protocol. Buffered
  speculation and routing controls must never appear in the conversation.
- Timeout or local failure follows `interceptor.fallback_on_error`.
  Sensitive rejection overrides that fallback and never continues externally
  after a screening failure.
- Optional `interceptor.redact` is the permissive counterpart to
  `reject_sensitive`: detected secrets become stable placeholders on the
  caller's own body before the local copy is derived from it, and the originals
  are restored as the response streams. Only values needing no JSON escaping
  are ever restored; anything else stays redacted rather than corrupting a
  body. Redaction never suppresses `reject_sensitive`.
- In-band routing verbs are recognised only at the very start of the newest
  user message, only against the closed set (`triage`, `unstick`, `local`,
  `default`, `remember`, `recall`, `forget`, and configured target names), and
  are stripped from every user turn before either copy exists. A leading
  backslash escapes one. A target may not be named after a verb while
  `integrated.verbs` is on.
- Integrated launcher routes, including explicit-model and `ROUTE_DEFAULT`
  traffic, strip caller credentials; every external destination must have a
  proxy-owned credential.
- Codex control requests bypass interceptor/integrated local execution because
  they require native response items. Pure proxy sessions relay them directly
  to the default provider with only model remapping.

## Normal Routing

Without a launcher, preserve these rules:

- Explicit model aliases win.
- A recognized `provider/model` namespace selects that provider and strips only
  the first prefix. It bypasses integrated triage; standalone interception may
  still perform its independent `PASS` check.
- Otherwise the first upstream model is selected and cached.
- In integrated sessions, a non-`Auto` model that is not an alias or namespace
  is preserved on the default provider. It is locally checked only when the
  standalone interceptor is also enabled.
- Alias-covered requests skip model discovery.
- Exactly one integrated target may set `default = true`. `ROUTE_DEFAULT`
  resolves to it instead of falling through to first-upstream-model discovery;
  validation rejects a second one after a project overlay merges.
- Control tool names are matched case-insensitively with `-` and `_` treated
  alike, against the configured names rather than one forced spelling.
- Provider selection, role rewriting, and protocol adapters remain active.
- A configured provider `reasoning_effort` is an enforced outbound policy:
  Chat Completions uses `reasoning_effort`, Responses uses `reasoning.effort`,
  and Anthropic Messages uses `output_config.effort`.
- An integrated target's `reasoning_effort` overrides its provider default and
  is included in the dynamic routing prompt and admin route editor.
- `--intercept` alone keeps normal external remapping after `PASS`.
- Sticky routing is bounded by `sticky_absolute_ttl_secs` (measured from the
  decision and never refreshed) and `sticky_max_turns`, not only by the sliding
  idle TTL. `sticky_policy = "escalate"` lets a re-triage move up the `strength`
  ordering but never down.
- A target's `failover` chain is tried only after a non-success status, which
  means the provider generated nothing; a target whose provider is
  rate-limited or circuit-open is skipped while another candidate remains, and
  is withheld from the triage tool list unless withholding would leave none.
- Second opinions and shadow requests buffer both answers and render through
  the local-completion path. Either side asking for a tool call cancels the
  comparison, and a shadow copy never reaches the caller.
- Speculative triage requires a `default = true` target, never runs with
  `reject_sensitive` or an in-band verb, and its buffered output is discarded
  unless triage agrees with it.
- `[integrated.embedding]` computes one vector per request for the routing
  cache, sticky drift, and retry escalation. All three are disabled by
  `reject_sensitive`, and every embedding failure is soft.
- Provider `kind` selects OpenAI-compatible, OpenAI, Anthropic, or Copilot
  upstream adaptation. OpenAI Chat Completions remains the canonical format.

## Upstream And Streaming

- Extend the shared request pipeline in `src/main.rs`; do not create parallel
  endpoint-specific clients.
- Keep request IDs, failure classification, logging, retry safety, and response
  rendering consistent through the existing upstream metadata helpers.
- Never retry generation after it may have reached upstream. Retry only safe,
  idempotent requests.
- Do not invalidate the model cache for unrelated generation decode failures.
- Streaming requests use a response-header timeout, not a timeout covering the
  complete SSE body.
- Convert stream body failures into native protocol error events rather than
  closing sockets abruptly.
- Anthropic upstream Messages and typed SSE translate through the canonical
  chat pipeline. Copilot Claude models use its native `/v1/messages` wire.
- Signed reasoning/thinking artifacts are removed when crossing providers;
  tool-call and tool-result ids remain paired.
- Client-facing protocol changes require wire-level streaming coverage.

## Launchers

- `--proxy --codex|--claude|--copilot` is a tokenized pure-adapter session. It
  disables interceptor and integrated routing, clears aliases and non-default
  providers, strips caller credentials, and remaps every generation request to
  the configured default provider. It adds no system prompt or control tools.
- `--intercept --codex` uses native OpenAI Responses passthrough and preserves
  Codex models and request metadata only for its tokenized launcher session.
- `--intercept --claude` uses native Anthropic Messages passthrough and
  preserves Claude models, bodies, headers, and SSE only for its session.
- `--intercept --copilot` supports native Copilot model discovery, Chat
  Completions, Responses, and Anthropic-wire requests. GitHub MCP and remote
  sessions stay external; provider-specific BYOK variables must not bypass the
  launcher.
- `--integrated` launchers use tokenized sessions over the protocol adapters
  and default their wire model to `Auto` unless the user selected a model.
  Copilot uses its documented BYOK Responses surface; GitHub MCP and remote
  sessions remain external.
- `--codex`, `--claude`, or `--copilot` without a mode defaults to integrated;
  `--proxy`, `--intercept`, and `--integrated` remain explicit overrides.
- Native interceptor sessions leave credential loading and refresh with the
  matching agent. Proxy and integrated sessions do not require agent login and
  never forward caller-generated credentials.
- Launcher modes are mutually exclusive, loopback-only, and protected against
  upstream loops.
- A launcher resolves the nearest project `.llmproxy.toml` at or above the
  working directory (`--project` overrides, `--no-project` disables) and carries
  its path in the session token. The token is unsigned, so the daemon re-reads
  that file and re-validates it against its own providers; a project may define
  only `[integrated.targets.*]` and `[model_aliases]`, and only integrated
  sessions apply the overlay. Every mode records the project identity so
  traffic is attributed to it.
- Interactive launchers resume only when their native session behavior permits;
  explicit prompt, session, management, help, and version arguments are kept.

## Daemon And Admin

- Normal startup creates or reuses the managed background daemon.
- `--restart` replaces it and `--shutdown` stops it.
- The daemon publishes credential-free runtime metadata after the proxy and
  optional admin listeners bind. Startup prints the exact proxy, admin, and log
  locations.
- Each server instance writes full request/response TRACE events through a
  bounded asynchronous timestamped file writer under the llmproxy state
  directory. Backpressure preserves diagnostics instead of dropping them. HTTP
  failures, response-stream failures, and timeouts are ERROR events. Never log
  credential headers. Log files are owner-only. Startup removes only matching
  log files older than `max_logging_retention_days`; zero disables cleanup.
- Interceptor/integrated decisions emit a structured log event with the
  observable policy reason, requested model, triage model/latency, finalized
  provider/model/class, and fallback error. The admin keeps a bounded 24-hour
  decision feed with prompt previews; it never stores credential headers.
- Proxy, interceptor, and integrated launcher sessions can safely share one
  daemon because routing mode is encoded in each loopback-only session token.
- The admin interface is unauthenticated and must remain loopback-only.
- Runtime config updates must validate and build replacement providers before
  atomically swapping shared state; persist edits without discarding comments.
  A swap also reconfigures provider health and prunes routing-cache entries
  naming a target the edit removed.
- Durable per-project usage lives in `~/.llmproxy/usage.json`, month-bucketed
  and bounded, flushed on a timer and once more at graceful shutdown. It stores
  token counters only, never prices: a report prices them at read time, so a
  rate correction re-prices history instead of leaving a stale number behind.
- Project memory lives in `~/.llmproxy/memory/<project>.json`, owner-only, and
  is written and read by explicit direction only. Memory writes are refused
  while `reject_sensitive` is on, because a note outlives the request that
  carried it.
- Spend, leaderboard, and shadow reporting must keep their caveats attached: a
  subscription provider has no dollar figure, spend derived from estimated
  token counts is an approximation, and a rank below the sample-size floor is
  reported but never ranked.

## Source Map

- `src/main.rs`: CLI, daemon lifecycle, routes, upstream pipeline, adapters,
  launcher sessions, and startup output.
- `src/backend_adapters.rs`: provider-facing Anthropic and Copilot adapters and
  cross-provider history sanitization.
- `src/interceptor.rs`: local request construction, routing controls, decision
  audit drafts, and failure policy.
- `src/interceptor_render.rs`: native-protocol rendering of accepted local
  output, attached to `interceptor` via `#[path]`.
- `src/oauth.rs`: stored CLI credential reuse for `codex-oauth` and
  `claude-oauth` provider `auth` values.
- `src/logging.rs`: bounded asynchronous trace file setup, timestamp naming,
  and retention cleanup.
- `src/agent_launcher.rs`: shared launcher selection and lifecycle.
- `src/*_launcher.rs`: agent-specific discovery and environment injection.
- `src/admin.rs`: loopback admin API, UI serving, and runtime config swaps.
- `src/stats.rs`: request classification, latency, attempted frontend/provider/
  model routes, completed token and cumulative-time usage, traffic logs,
  bounded routing decision audits, and shadow comparisons.
- `src/pricing.rs`: per-provider price metadata and cache-aware spend
  arithmetic. Config is the only source of rates; nothing is hardcoded.
- `src/usage_store.rs`: durable month-bucketed per-project usage and outcome
  counters.
- `src/leaderboard.rs`: per-project model ranking, proposed hints from observed
  clusters, and the cost insight built on both.
- `src/health.rs`: per-provider health, circuit breaking, and quota tracking.
- `src/project.rs`: project `.llmproxy.toml` discovery, parsing, and overlay
  validation.
- `src/routing_verbs.rs`: in-band verb detection and stripping.
- `src/route_cache.rs`: embedding-cached routing, sticky drift, and retry
  detection.
- `src/route_corpus.rs`: routing corpus capture, replay comparison, and the
  regression summary behind `--route-corpus`.
- `src/comparison.rs`: second opinions and shadow-request sampling.
- `src/redact.rs`: secret detection, placeholder substitution, and streaming
  rehydration.
- `src/memory.rs`: cross-agent shared project memory.
- `ui/`: task-oriented admin interface organized as Overview, Activity,
  Routing, Models, Providers, and Settings; Activity and Routing use focused
  secondary views — Activity > Usage, Spend, Decisions, Shadow, and Event log,
  and Routing > Triage setup, Policy & test, and Project memory.
- `ui/help.html`: the user guide, served at `/help.html` by the same static
  fallback as the rest of `ui/` and linked from the masthead of every admin
  page. One card per feature, each answering what it is, why it was built, how
  to switch it on, how to drive it, where to watch it, and what it costs. It
  calls no API, so it stays readable when the proxy is unhealthy. Keep it in
  step with the settings it names; the recorded reasoning behind each feature
  lives in `ROADMAP.md`.
- `llmproxy.example.toml`: public credential-free configuration reference.
- `ROADMAP.md`: candidate work and recorded design decisions, grouped by
  what each item delivers. Nothing in it is committed work.

## Editing And Verification

- Use `rg` for searches and `apply_patch` for manual edits.
- Unit tests live in sibling `src/<module>_tests.rs` files attached with
  `#[path = "<module>_tests.rs"] mod tests;`, not inline in the module. The
  exception is `src/main.rs`, which keeps an inline `mod tests`. Add new tests
  to the matching sibling file.
- Normal Cargo commands build only for the current host. Do not add global
  `target-cpu=native` flags; release artifacts must run across CPUs of the same
  architecture. CI validates native x86_64 and ARM64 builds on Linux, macOS,
  and Windows.
- For routing, interceptor, daemon, admin, or launcher changes, run:

  ```sh
  cargo fmt -- --check
  cargo test
  cargo build --release
  ```

- Use isolated mock upstreams for credential and fallback smoke tests. Never
  log or persist real authorization values.
- Strict Clippy may expose older warnings in `src/main.rs`; distinguish them
  from warnings introduced by the current change.
- Update this file after material architecture or launcher changes. Do not add
  session IDs, local filesystem paths, personal identifiers, or live topology.
