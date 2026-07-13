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
                                               \-> PASS -> original external request
```

- The interceptor receives a separate dynamic request with compatible tools
  and a zero-argument `PASS` function. It must never mutate the external copy.
- `PASS` is valid only as the sole tool call or exact text output. Mixed output
  is invalid.
- Caller authentication, account headers, and external credentials must never
  reach the local provider.
- Accepted local output is rendered in the caller's native protocol. Buffered
  speculation and `PASS` must never appear in the conversation.
- Timeout or local failure follows `interceptor.fallback_on_error`.
- Codex control requests, including compaction triggers and non-turn request
  kinds, bypass local execution because they require native response items.

## Normal Routing

Without a launcher, preserve these rules:

- Explicit model aliases win.
- Otherwise the first upstream model is selected and cached.
- Alias-covered requests skip model discovery.
- Provider selection, role rewriting, and protocol adapters remain active.
- `--intercept` alone keeps normal external remapping after `PASS`.

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
- Client-facing protocol changes require wire-level streaming coverage.

## Launchers

- `--intercept --codex` uses native OpenAI Responses passthrough and preserves
  Codex models and request metadata only for its tokenized launcher session.
- `--intercept --claude` uses native Anthropic Messages passthrough and
  preserves Claude models, bodies, headers, and SSE only for its session.
- `--intercept --copilot` supports native Copilot model discovery, Chat
  Completions, Responses, and Anthropic-wire requests. GitHub MCP and remote
  sessions stay external; provider-specific BYOK variables must not bypass the
  launcher.
- Each agent owns credential loading and refresh. Caller-generated credentials
  are accepted only on loopback and forwarded only to the external provider.
- Launcher modes are mutually exclusive, loopback-only, and protected against
  upstream loops.
- Interactive launchers resume only when their native session behavior permits;
  explicit prompt, session, management, help, and version arguments are kept.

## Daemon And Admin

- Normal startup creates or reuses the managed background daemon.
- `--restart` replaces it and `--shutdown` stops it.
- The daemon publishes credential-free runtime metadata after the proxy and
  optional admin listeners bind. Startup prints the exact proxy and admin URLs.
- The admin interface is unauthenticated and must remain loopback-only.
- Runtime config updates must validate and build replacement providers before
  atomically swapping shared state; persist edits without discarding comments.

## Source Map

- `src/main.rs`: CLI, daemon lifecycle, routes, upstream pipeline, adapters,
  launcher sessions, and startup output.
- `src/interceptor.rs`: local request construction, `PASS`, failure policy, and
  local response rendering.
- `src/agent_launcher.rs`: shared launcher selection and lifecycle.
- `src/*_launcher.rs`: agent-specific discovery and environment injection.
- `src/admin.rs`: loopback admin API, UI serving, and runtime config swaps.
- `src/stats.rs`: request classification, latency, usage, and traffic logs.
- `ui/`: admin interface assets.
- `llmproxy.example.toml`: public credential-free configuration reference.

## Editing And Verification

- Use `rg` for searches and `apply_patch` for manual edits.
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
