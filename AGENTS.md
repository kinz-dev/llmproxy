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

`llmproxy` is a provider-routing Rust proxy for OpenAI Chat Completions and
Responses, Anthropic Messages, and native agent launcher traffic.

```text
agent request -> separate triage-provider request -> deterministic response (intercept only)
                                                  \-> REJECT -> safe no-action response
                                                  \-> PASS -> normal configured routing
                                                  \-> Auto + configured route -> provider/model
```

- The triage provider receives a separate dynamic request with compatible tools.
  Model `Auto` enables integrated `PASS` and configured route tools.
  Other models bypass integrated triage. Intercept mode advertises `PASS`
  independently of the requested model. The triage copy must never mutate the
  original execution copy.
- Routing controls are valid only as the sole tool call or exact text output.
  Mixed output is invalid. Integrated mode advertises `PASS` and continues to
  accept legacy `ROUTE_DEFAULT`. Tool calls may carry only a concise `reason` audit
  field; legacy empty arguments remain valid, and audit metadata is never
  rendered to the caller.
- Optional `interceptor.reject_sensitive` adds a mandatory, dynamically
  appended `REJECT` control before execution/provider routing. The built-in
  policy selects it at 75% sensitive-data likelihood or higher. Its reason is
  a safe category, never the detected value. The triage provider must be on a
  loopback or private network because it sees the prompt before classification.
  Rejection renders a native successful
  no-action response, disables sticky bypass, and fails closed when screening
  cannot complete. Screened request/triage bodies are omitted from TRACE, and
  rejected audit prompts are redacted. Proxy mode ignores it and remains
  prompt-free.
- Caller authentication, account headers, and execution-provider credentials must never
  reach the triage provider.
- Accepted triage-provider output is rendered in the caller's native protocol.
  The built-in intercept policy executes directly only above 80% deterministic
  confidence. Integrated triage is router-only and direct output is invalid. Buffered
  speculation and routing controls must never appear in the conversation.
- Timeout or triage-provider failure follows `interceptor.fallback_on_error`.
  Sensitive rejection overrides that fallback and never continues to another
  provider after a screening failure.
- Integrated launcher routes, including explicit-model and `PASS`
  traffic, strip caller credentials; every remote destination must have a
  proxy-owned credential.
- Codex control requests bypass interceptor/integrated triage execution because
  they require native response items. Pure proxy sessions relay them directly
  to the default provider with only model remapping.

## Normal Routing

Without a launcher, preserve these rules:

- Explicit model aliases win.
- A recognized `provider/model` namespace selects that provider and strips only
  the first prefix. It bypasses integrated triage; intercept mode still performs
  its independent `PASS` check.
- Otherwise the first upstream model is selected and cached.
- In integrated sessions, a non-`Auto` model that is not an alias or namespace
  is preserved on the default provider and bypasses triage.
- Alias-covered requests skip model discovery.
- Provider selection, role rewriting, and protocol adapters remain active.
- A configured provider `reasoning_effort` is an enforced outbound policy:
  Chat Completions uses `reasoning_effort`, Responses uses `reasoning.effort`,
  and Anthropic Messages uses `output_config.effort`.
- An integrated target's `reasoning_effort` overrides its provider default and
  is included in the dynamic routing prompt and admin route editor.
- Intercept mode keeps normal provider remapping after `PASS`.
- Provider `kind` selects OpenAI-compatible, OpenAI, Anthropic, or Copilot
  upstream adaptation. OpenAI Chat Completions remains the canonical format.

## Failover And Rules

- A provider's `fallback` list names the providers to try when it is
  unavailable. Entries are a provider name or `provider/model`. Chains are one
  level deep and deduplicated; fallbacks are never followed recursively.
- Failover is allowed only for availability failures (connect, timeout, 408,
  429, 502, 503, 504) and only while no bytes have reached the caller. Body and
  decode failures and 5xx other than the listed gateway codes are never failed
  over, because the upstream may already have generated and billed.
- Hops after the first strip cross-provider artifacts and re-normalize the
  request. Every hop recomputes provider credentials; a fallback never inherits
  the previous provider's headers.
- Statistics and pricing record the provider and model that actually served,
  not the one originally routed to.
- Each provider has a circuit breaker. After
  `circuit_breaker_threshold` consecutive failures it opens, is skipped for
  `circuit_breaker_cooldown_secs`, then admits exactly one probe. A failed
  probe restarts the cooldown. Threshold 0 disables it. Native launcher
  external providers carry no fallbacks and no breaker.
- Integrated redirects do not fail over: the destination is an audited triage
  decision, and silently serving it elsewhere would make that audit false.
- `[[routing_rules]]` are evaluated in order before any triage request, and the
  first match wins. Conditions left unset are not constraints, so a rule with
  only a `route` is a catch-all. `route` is `PASS`, an `[integrated.targets.*]`
  name, or `provider/model`; unknown routes fail validation.
- Rules never run before sensitive screening. When `reject_sensitive` is on,
  the prompt is classified before anything decides where it goes.
- A rule match is recorded in the decision audit under its rule name, with no
  triage provider or latency, so a deterministic route is distinguishable from
  a model-chosen one.

## Cost

- `[pricing.models]` holds USD per million tokens, keyed by `model` or
  `provider/model`; the namespaced key wins. `pricing.baseline_model` must
  itself be priced.
- A model with no rate is counted as unpriced, never as free, so a partial
  price table cannot understate spend. Triage requests are priced like any
  other call because intercept mode really does add one per screened request.

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

- `routing_mode` is exclusive per instance: `proxy`, `intercept`, or
  `integrated`. `--codex`, `--claude`, and `--copilot` always inherit the live
  instance mode. Change it with config, the admin, or `--restart --mode`; mode
  flags cannot be combined with a launcher.
- Proxy launchers are tokenized pure-adapter sessions. They clear aliases and
  non-default providers, strip caller credentials, and remap every generation
  request to the default provider. They add no prompt or control tools.
- Intercept Codex and Claude launchers preserve their native Responses or
  Messages wire, models, request metadata, headers, and SSE. Copilot intercept
  supports native model discovery, Chat Completions, Responses, and
  Anthropic-wire requests. GitHub MCP and remote sessions stay external.
- Integrated launchers use tokenized sessions over the protocol adapters and
  default their wire model to `Auto` unless the user selected a model. Copilot
  uses its documented BYOK Responses surface; GitHub MCP and remote sessions
  remain external.
- Native interceptor sessions leave credential loading and refresh with the
  matching agent. Proxy and integrated sessions do not require agent login and
  never forward caller-generated credentials.
- On Windows, launcher discovery resolves supported `.exe`, `.com`, `.cmd`, and
  `.bat` files from `PATH` before authentication probes or interactive spawn.
- Launcher sessions are loopback-only, reject tokens from a different instance
  mode, and are protected against upstream loops.
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
- The admin can atomically change the instance routing mode and schedule a
  managed restart. A restart stops the current bound listener and applies the
  validated configured fixed listener address. Existing agent processes must be
  relaunched after a mode change because session tokens record the mode that
  created them.
- The admin interface is unauthenticated and must remain loopback-only.
- Admin requests are admitted by Host/Origin resolution, not by literal spelling:
  the Host port must equal the bound port and the host must resolve entirely to
  loopback, so `localhost` and a `127.0.1.1` machine name work while an
  attacker-chosen hostname still cannot. A name yielding any routable address
  is rejected outright, and a non-resolving name fails closed. Never replace
  this with a hostname allowlist or a removed check: the Host check is the one
  that still holds under DNS rebinding, after which Origin is same-origin.
- Runtime config updates must validate and build replacement providers before
  atomically swapping shared state; persist edits without discarding comments.

## Source Map

- `src/main.rs`: CLI, daemon lifecycle, routes, upstream pipeline, adapters,
  launcher sessions, and startup output.
- `src/backend_adapters.rs`: provider-facing Anthropic and Copilot adapters and
  cross-provider history sanitization.
- `src/interceptor.rs`: triage request construction, routing controls, decision
  audit drafts, failure policy, and native response rendering.
- `src/logging.rs`: bounded asynchronous trace file setup, timestamp naming,
  and retention cleanup.
- `src/agent_launcher.rs`: shared launcher selection and lifecycle.
- `src/*_launcher.rs`: agent-specific discovery and environment injection.
- `src/admin.rs`: loopback admin API, UI serving, and runtime config swaps.
- `src/stats.rs`: request classification, latency, attempted frontend/provider/
  model routes, completed token and cumulative-time usage, per-model cost
  against configured rates, failover/breaker counters, traffic logs, and
  bounded routing decision audits.
- `ui/`: task-oriented admin interface organized as Overview, Activity,
  Routing, Models, Providers, and Settings; Activity and Routing use focused
  secondary views, including Activity > Decisions for policy audits.
- `llmproxy.example.toml`: public credential-free configuration reference.

## Editing And Verification

- Use `rg` for searches and `apply_patch` for manual edits.
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
