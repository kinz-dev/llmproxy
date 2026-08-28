# llmproxy

`llmproxy` is a local-first Rust proxy for Codex, Claude Code, Copilot CLI,
OpenAI Chat Completions/Responses, and Anthropic Messages. It adapts requests to
OpenAI-compatible, OpenAI, Anthropic, Copilot, or local providers.

## Start

```sh
cargo build --release
cp llmproxy.example.toml config.toml
# edit config.toml, then:
./target/release/llmproxy
```

`cargo build` targets only the current OS and CPU architecture. CI builds and
tests native x86_64 and ARM64 binaries on Linux, macOS, and Windows.

Startup creates or reuses a background daemon and prints the proxy, optional
loopback admin, and diagnostic log locations. Config discovery is
`--config`, `LLMPROXY_CONFIG`, `./config.toml`, then
`~/.llmproxy/config.toml`. Local `config.toml` is ignored by Git.

## Agent Modes

```sh
llmproxy --codex                 # integrated (default when mode is omitted)
llmproxy --claude
llmproxy --copilot

llmproxy --proxy --codex        # protocol adapter only; no routing prompt
llmproxy --intercept --codex    # local answer or native-provider PASS
llmproxy --integrated --codex   # explicit form of the default
```

`--proxy` sends every generation to the default provider. `--intercept` asks a
separate local model first. `--integrated` sets the agent model to `Auto`, which
lets the local triage model choose `[integrated.targets.*]`; explicit models
bypass triage unless standalone `[interceptor] enabled = true`.
Optional `interceptor.reject_sensitive = true` screens first and returns a
native no-action response instead of sending sensitive prompts externally;
screened TRACE bodies and rejected audit prompts are redacted.

Aliases and `provider/model` IDs select exact routes. Cross-provider calls use
only proxy-owned credentials. Provider `reasoning_effort` sets a default;
integrated targets can override it per model, for example
`reasoning_effort = "max"` on a research route. Stored CLI logins can be reused
with `auth = "codex-oauth"` or `auth = "claude-oauth"`.

## Routing Controls

Sticky routing keeps one conversation on one provider. The idle TTL slides on
every turn, so `sticky_absolute_ttl_secs` and `sticky_max_turns` are what
reconsider a route during an active session; `sticky_policy = "escalate"` then
lets a re-triage move a session up to a stronger route but never back down.

Agent CLIs give no way to set a per-message header, so a small closed set of
in-band verbs is recognised at the very start of the newest user message and
stripped before anything is forwarded: `triage:` re-runs routing, `unstick:`
drops the pin, `local:` answers here, `default:` takes the catch-all, and any
target name pins that route with no triage latency at all. `remember:`,
`recall:`, and `forget:` manage the shared project memory. A leading backslash
sends one literally.

Each target may name a `failover` chain, so quota exhaustion and outages are
absorbed and reported in the admin interface rather than to the caller;
per-provider health tracking routes around a provider that is rate-limited or
down. A target may also ask for a `second_opinion`, where two providers answer
and both are shown or a third reconciles them.

With an embeddings provider configured, `[integrated.embedding]` adds a routing
cache that replays a near-identical past decision without calling the triage
model, re-triage when a conversation drifts from the request that pinned its
route, and escalation when the user re-asks the same question.

## Projects, Spend, and Evidence

A repository may carry a `.llmproxy.toml` defining `[integrated.targets.*]` and
`[model_aliases]` — and nothing else. Providers and credentials stay
user-level, so cloning an untrusted repository cannot decide where prompts are
sent. A launcher finds the nearest one at or above the working directory and
the daemon re-reads and re-validates it, so one daemon still serves every
project.

`[providers.*.pricing]` gives each model a per-million-token rate, including
cache reads and writes, and token counters are kept in a durable month-bucketed
ledger under `~/.llmproxy`, so spend survives a restart and breaks down by
project. A provider that bills against a plan reports token volume and never a
dollar figure. The admin interface adds a per-project model leaderboard scored
on observed aftermath — immediate re-asks and failures — plus sampled shadow
requests that record two providers' answers side by side.

`llmproxy --route-corpus corpus.json` replays a captured corpus of real
requests against the current policy and exits non-zero when a decision moved,
which makes a policy edit falsifiable in CI. Capture one from the admin
interface or from `GET /api/route-corpus`.

`[interceptor.redact]` is the permissive counterpart to `reject_sensitive`:
detected secrets become stable placeholders, the request routes normally, and
the originals are put back as the response streams.

Each daemon writes full request/response TRACE diagnostics to
`~/.llmproxy/llmproxy-yyyymmddhhmmss.log`; HTTP errors and timeouts are ERROR
events. `max_logging_retention_days = 7` controls cleanup at startup, while `0`
keeps logs indefinitely. Logs use `0600` permissions on Unix and the user's
inherited profile ACL on Windows; they never include authorization headers.
The loopback admin's Activity > Decisions view shows why triage chose local or
external routing, rejected a prompt, and the finalized provider/model. The
**Help** link in its masthead opens a full user guide at `/help.html`: one card
per feature covering what it is, why it was built, how to switch it on, how to
drive it, where to watch it, and what it costs.

```sh
llmproxy              # start or reuse
llmproxy --restart    # replace
llmproxy --shutdown   # stop
```

See [`llmproxy.example.toml`](llmproxy.example.toml) for all configuration.
