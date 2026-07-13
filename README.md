# llmproxy

`llmproxy` is a small Rust proxy that lets a fast local model handle simple
agent work and forwards harder requests unchanged to the agent's native
provider. It supports OpenAI Chat Completions and Responses, Anthropic Messages,
Codex, Claude Code, and GitHub Copilot CLI.

## Start

```sh
cargo build --release
cp llmproxy.example.toml config.toml
# edit config.toml, then:
./target/release/llmproxy --config config.toml
```

Startup runs a background daemon and prints the exact proxy URL. When `[admin]`
has `enable = true`, it also prints the admin page URL to open in a browser.
`config.toml` is ignored so credentials stay local.

## Launch Agents

```sh
llmproxy --intercept --codex
llmproxy --intercept --claude
llmproxy --intercept --copilot
```

Each launcher keeps the agent's existing login. The local interceptor receives
no caller credentials. A sole local `PASS` forwards the original model,
request, headers, and native streaming protocol externally.

## Control

```sh
llmproxy              # start or reuse the daemon
llmproxy --restart    # replace it
llmproxy --shutdown   # stop it
```

See [`llmproxy.example.toml`](llmproxy.example.toml) for providers, aliases,
interceptor settings, and the loopback-only admin interface.
