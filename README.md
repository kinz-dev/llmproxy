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

## Failover

A provider can name the providers to try when it is unavailable:

```toml
[providers.remote]
fallback = ["local/qwen3-coder"]
circuit_breaker_threshold = 5
circuit_breaker_cooldown_secs = 30
```

Failover applies to availability failures only (connect, timeout, 408, 429,
502, 503, 504) and only before any bytes reach the caller, so a request that
may already have generated is never sent twice. Fallbacks are not followed
recursively. After enough consecutive failures a provider's breaker opens and
it stops being tried at all until one probe succeeds again.

## Routing rules

Deterministic routes run before any triage request, so an obvious destination
costs no extra model round-trip:

```toml
[[routing_rules]]
name = "small no-tool work stays local"
max_input_tokens = 800
has_tools = false
route = "PASS"
```

Rules are ordered and the first match wins; `route` is `PASS`, an
`[integrated.targets.*]` name, or `provider/model`. Matches appear in the
decision audit under the rule name. Sensitive screening is never skipped.

## Cost

Per-model rates turn the token counters into spend:

```toml
[pricing]
baseline_model = "openai/gpt-4o"

[pricing.models."openai/gpt-4o"]
input_per_mtok = 2.50
output_per_mtok = 10.00
```

Keys are `model` or `provider/model`, the namespaced key winning. Unpriced
models are reported as unpriced rather than counted as free, and
`baseline_model` gives the counterfactual local routing is trying to move.

```sh
llmproxy             # start or reuse
llmproxy --restart   # replace
llmproxy --shutdown  # stop
```

See [`llmproxy.example.toml`](llmproxy.example.toml) for the complete,
credential-free reference. Builds target the current OS and architecture; CI
checks Linux, macOS, and Windows on x86_64 and ARM64.
