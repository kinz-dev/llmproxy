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

Aliases and `provider/model` IDs select exact routes. Cross-provider calls use
only proxy-owned credentials. Provider `reasoning_effort` sets a default;
integrated targets can override it per model, for example
`reasoning_effort = "max"` on a research route. Stored CLI logins can be reused
with `auth = "codex-oauth"` or `auth = "claude-oauth"`.

Each daemon writes full request/response TRACE diagnostics to
`~/.llmproxy/llmproxy-yyyymmddhhmmss.log`; HTTP errors and timeouts are ERROR
events. `max_logging_retention_days = 7` controls cleanup at startup, while `0`
keeps logs indefinitely. Logs are owner-only (`0600`) and contain prompt and
response content, but never authorization headers.

```sh
llmproxy              # start or reuse
llmproxy --restart    # replace
llmproxy --shutdown   # stop
```

See [`llmproxy.example.toml`](llmproxy.example.toml) for all configuration.
