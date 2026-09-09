# llmproxy

`llmproxy` is a local provider router and protocol adapter for Codex, Claude
Code, Copilot CLI, OpenAI Chat Completions/Responses, and Anthropic Messages.

## Start

```sh
cargo build --release
cp llmproxy.example.toml config.toml
./target/release/llmproxy
```

One `routing_mode` applies to the entire instance:

- `proxy`: send requests to the default provider without a routing prompt.
- `intercept`: let the triage provider answer deterministic work or `PASS`.
- `integrated`: route model `Auto` among `[integrated.targets.*]`; explicit
  models and `provider/model` IDs bypass triage.

Launchers inherit the running instance mode:

```sh
llmproxy --codex
llmproxy --claude
llmproxy --copilot
llmproxy --restart --mode integrated   # change the instance mode
```

Config discovery is `--config`, `LLMPROXY_CONFIG`, `./config.toml`, then
`~/.llmproxy/config.toml`. Startup prints the proxy, loopback admin, and log
locations. The admin can change routing mode, inspect decisions, and restart
the managed proxy.

Aliases route ordinary or embedding models. Provider and integrated-target
`reasoning_effort` enforce outbound reasoning policy. Optional private-network
sensitive screening rejects likely credentials or personal data before
routing; proxy mode remains prompt-free. Credentials are stripped from local
triage and cross-provider routes use proxy-owned credentials.

```sh
llmproxy             # start or reuse
llmproxy --restart   # replace
llmproxy --shutdown  # stop
```

See [`llmproxy.example.toml`](llmproxy.example.toml) for the complete,
credential-free reference. Builds target the current OS and architecture; CI
checks Linux, macOS, and Windows on x86_64 and ARM64.
