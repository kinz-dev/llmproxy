# llmproxy Roadmap

Candidate work, grouped by what it delivers. The ordering inside each group is
a rough preference, not a schedule.

Status: `idea` · `designed` · `building` · `done`

Everything below is currently `done`. What is recorded here is why each item
was built the way it was, and what is left open — not what remains to build.
Configuration for all of it is documented in `llmproxy.example.toml`; the
invariants each feature must keep are in `AGENTS.md`.

## End-User Features

These exploit llmproxy's position: it is the only component that sits across
every agent, every provider, and every session, and it sees complete requests
and responses locally.

### Cross-agent shared project memory — `done`

Each agent CLI keeps its own history and shares nothing with the others. The
proxy sees all of them, so a project-scoped store of prior findings, decisions,
and dead ends is injected into whichever agent is running.

Two limits are deliberate and worth keeping. The store is written and read by
explicit direction only — the `remember:`, `recall:`, and `forget:` verbs, or
the admin API — because nothing else can be undone: a wrong inference would be
silently prepended to every later request in the project. And the injected
block is hard-bounded and labelled as prior context rather than instructions,
because it is paid for on every turn and read by a model that will otherwise
treat it as a directive.

Memory writes are refused while `reject_sensitive` is on. A note outlives the
request that carried it, so an unscreened one would turn a single exposure into
a standing one.

Open: nothing captures memory automatically. Automatic extraction is the
obvious next step and the obvious way to poison the store; it wants an explicit
review step before anything reaches injection.

### Retry-driven escalation — `done`

Re-asking the same question a different way is a dissatisfaction signal, and it
is visible here: same conversation, near-identical embedding, no intervening
tool use. The retry moves the request up one rank in the `strength` ordering,
silently, with no new setting to reason about at the call site.

The "no intervening tool use" condition is what keeps it honest. A similar
follow-up after the agent has run tools is continuation, not dissatisfaction,
and treating it as the latter would escalate ordinary iteration.

Requires `[integrated.embedding]`, since the comparison is an embedding
distance. The same signal is recorded as an outcome for the leaderboard.

### Transparent quota and outage failover — `done`

Rate-limit exhaustion mid-task is the sharpest failure in agent-assisted work,
and the proxy owns every provider credential, so it can absorb it: exhausted on
A, continue on B, reported in the admin interface rather than to the caller.

Failover runs only on a non-success status, which means the provider rejected
the request before generating anything — so nothing is billed or produced
twice. Quota exhaustion and outages are tracked separately because they need
different recoveries: a 429 carries a duration the provider tells us about,
while an outage gets exponential backoff behind a single half-open probe.

### Second opinion on consequential requests — `done`

Opt in per target: two providers answer the same request and both are shown, or
a third reconciles them while being told to name the disagreements rather than
smooth them over. Agreement is signal; divergence is the stronger one.

A tool call on either side cancels the comparison and the request is sent
normally, because a composed answer is text and would silently drop the call.
The agreement score shown beside the answers is a word overlap, not a semantic
judgement, and is labelled as such.

Open: it costs two answers and gives up streaming for the requests it touches.
That is the right trade for a design decision and the wrong one for everything
else, which is why it is per-target rather than global.

### Personal model leaderboard — `done`

Public benchmarks rank models on generic tasks. This ranks them on real work in
a real repository, using the aftermath the proxy can see: an immediate re-ask
with no intervening tool use, an error, a broken stream. That is free implicit
labelling, and it yields a per-project ranking rather than an average one.

Rows below a sample-size floor are shown but never ranked — a rank derived from
four requests is noise wearing a number's clothes. Cost breaks ties rather than
driving the order, because a cheap route that gets re-asked is not cheap.

Proposed hints come from the terms that distinguish one target's observed
prompts from the rest. It is a frequency contrast, not a topic model, and it is
offered for a human to edit rather than applied.

Open: the signal set is small. Whether an edit was reverted is the strongest
label available and the proxy cannot see it without watching the repository.

### Spend meter and cost insight — `done`

Live per-project spend, priced from `[providers.*.pricing]`, plus the insight
built on it: a routing change is proposed only when the cheaper route has
enough evidence behind it and is not observably worse, and the suggestion
carries the exact payload the routing editor takes.

Cost alone is never a reason to move. Pairing the spend figure with observed
satisfaction is what stops the insight from recommending its way into a worse
outcome.

## Admin UI

### Per-provider flow ribbons — `done`

The overview flow diagram derives its ribbons from `served_by_provider` instead
of two hardcoded paths. The design decisions, recorded because they are easy to
get wrong:

- **Colour stays bound to locality, not provider identity.** The whole UI
  encodes local/external through `--local` and `--external`; per-provider hues
  would break that and require validating a fresh categorical palette. Identity
  is carried by stable position and a direct label, so it is never colour-alone.
- **Ribbon order is stable, never sorted by volume.** Sorting by request count
  makes providers swap position on every poll. Group by locality, then sort
  alphabetically; only thickness varies.
- **Cap the fan-out.** Vertical room allows roughly six ribbons before labels
  collide. Beyond that, the tail folds into one aggregate ribbon.
- **Update keyed, not wholesale.** The drift animation restarts if the nodes are
  rebuilt each poll, which visibly stutters. Each provider's node is reused.

### Per-model spend analytics — `done`

The per-model token counters are priced, and each of the four things that would
have made the number untrustworthy is handled explicitly:

- **Price metadata.** `[providers.*.pricing]` is keyed by model under each
  provider, with separate input, output, cache-read, and cache-write rates, per
  million tokens. Rates change, so config is the only source of truth and
  nothing is hardcoded. An exact model id wins, then the longest configured
  prefix, then `*`.
- **Cached tokens.** `cached_tokens` and cache creation now reach stats through
  every protocol adapter, normalised onto one convention: the prompt count
  includes its cached share, as OpenAI reports it, with Anthropic's separate
  fields summed back in. Cache reads bill at a steep discount and writes at a
  premium, so a naive input-token multiplication overstated agent traffic badly.
- **Estimated counts are labelled.** Spend derived from `estimated_requests` is
  reported as an approximation rather than presented as a precise figure.
- **Counters survive a restart.** A month-bucketed ledger under `~/.llmproxy`
  keeps token counters durably and bounded. It stores counters, never prices, so
  a rate correction re-prices history instead of leaving a stale number behind.
- **Subscription providers are not per-token.** `subscription = true` reports
  token volume and no dollar figure, because a figure there would be a fiction.

## Routing

### Project-scoped routing overlay — `done`

Per-project semantic route tables, so a project can send development work to one
provider, front-end work to another, and everything else to a third, without any
project identity reaching the triage model.

- A project file defines `[integrated.targets.*]` and `[model_aliases]` only,
  and every provider it names must already exist in the user-level config.
  Anything else is rejected by name. Providers, base URLs, and credentials stay
  user-level: a project file lives in a repository, and cloning an untrusted
  repository must not be able to define where prompts are sent.
- Project targets **merge** with user-level targets; the project wins on a name
  collision, and nothing user-level is removed.
- A `default = true` field marks the catch-all target. Validation rejects more
  than one after merge, and `ROUTE_DEFAULT` resolves to it in code instead of
  falling through to first-upstream-model discovery.
- The scope reaches the daemon as a path inside the launcher session token, so
  one daemon continues to serve every project. The token is unsigned, so the
  daemon re-reads the file at that path and re-validates it against its own
  providers rather than trusting anything the token asserted.

Open: hint overlap between a broad route and a narrower one still has no
tie-break. Disjoint hint wording works; an explicit `priority` field on targets
remains the structural fix if that wording grows unwieldy.

### Sticky routing bounds — `done`

The sticky TTL was a sliding window: `conversation_key` prefers a session header
that agent CLIs send on every turn, and a successful redirect re-inserted the
entry, so the first message of a session chose the route and the choice survived
for as long as the user kept working. No value of `sticky_ttl_secs` changed
that, because the TTL only ever governed idle gaps.

All four candidate directions were built, because they bound different things:

- **Absolute TTL.** The insertion timestamp survives a refresh, so a long
  session is re-triaged periodically even under continuous use.
- **Turn bound.** A sticky entry serves at most N turns before routing is
  reconsidered.
- **Re-triage on material change.** When the embedding cache is on, a request
  far enough from the one that pinned the route reconsiders it.
- **Escalate but never demote.** A later turn may move up the `strength`
  ordering but not down, so cost control survives without pinning quality to
  whatever the first message happened to be.

The Decisions view distinguishes a sticky reuse from a fresh decision, and names
the bound that expired.

### In-band routing verbs — `done`

Agent CLIs give the user no way to set a per-message header, so the message text
is the only channel back into routing. A closed verb set is recognised at the
very start of the newest user message: `triage:` re-runs routing, `unstick:`
drops the pin, `local:` answers here with the routing tools withheld,
`default:` takes the catch-all, and a target name pins that route directly —
deterministic and free of triage latency.

Everything that had to be settled at implementation time, and how:

- **Where detection happens.** In the protocol handler, on the caller's native
  body, before the sticky lookup and before the local copy is derived. A sticky
  entry can therefore never short-circuit a verb.
- **Stripping both copies.** The strip is applied to the caller's own body, and
  the local copy is built from the stripped result, so the prefix reaches
  neither the provider nor the triage model.
- **Stale prefixes in history.** Recognised prefixes are stripped from every
  user turn, not only the newest, so the triage model cannot read an old
  directive as if it were current.
- **Collision safety.** Only at the start of the newest user message, only an
  exact match against the closed list, with a leading backslash as the escape.
  A target may not be named after a verb while verbs are on.
- **Verb vocabulary.** The four routing verbs above, plus `remember:`,
  `recall:`, and `forget:` for project memory. Target names are already
  constrained to tool-name-safe characters, so they parse unambiguously.

### Embedding-cached routing — `done`

`RoutingDecisionAudit` records prompt, decision, target, and reason for every
request. Embedding the request and matching it against past decisions turns that
into a routing cache: triage cost drops, the hit path costs one local embedding,
and identical requests route identically by construction.

Only replayable outcomes are stored. A rejection or a malformed control is
always re-derived, and a cached decision naming a target a config edit removed
is pruned rather than replayed. Sensitive screening disables the cache
entirely, since a cached decision would skip screening that must fail closed.

Open: the end state is distillation, with the LLM tier as the fallback rather
than the default. The cache is the first half of that; nothing yet trains on
what it accumulates.

### Redact and route — `done`

`reject_sensitive` was binary: detect and refuse, which turns "there is a
credential in this file" into "you cannot use a good external model on this code
at all". Substituting stable placeholders, routing normally, and rehydrating on
the way back converts that into "you can".

The hard part was rehydration, and it is handled by narrowing rather than by
cleverness. Only values that need no JSON escaping are ever spliced back, so a
placeholder inside a tool-call argument string is safe; a private-key block is
redacted and stays redacted rather than corrupting a body. The streaming
rehydrator holds back a chunk tail so a split placeholder is still matched, and
hands the text layer only complete UTF-8.

Detection is prefix- and context-driven rather than entropy-scored. A false
positive silently damages a prompt, which is worse than a miss.

### Speculative triage — `done`

The triage call and the presumed-default route are issued concurrently and the
loser is cancelled, which takes the triage round trip off the critical path at
the cost of some wasted tokens. Buffered speculation never reaches the
conversation: the task is aborted and its output dropped unless triage agrees.

It never runs with sensitive screening on — that would put the prompt in front
of a provider before it had been screened — and never when an in-band verb has
already decided the route.

### Provider failover and circuit breaking — `done`

`fallback_on_error` was one-dimensional: local to external. Per-provider health
is the missing axis. A provider that is rate-limited or circuit-open is routed
around, and is withheld from the triage tool list so the model picks a working
route instead of one that would only fail over afterwards — unless withholding
would leave nothing, since an empty route list is worse than an optimistic one.

## Platform

### Routing regression suite — `done`

Single-shot route testing answers "where would this one prompt go". The corpus
version answers the question that matters when a policy is edited: "what did
this change move". Real requests are captured from the decision audit, replayed
against an edited prompt or target set, and diffed.

Decisions that mean the same destination compare as one — a sticky reuse and a
cached replay are both "it went to that target" — and a new case is not a
regression. `llmproxy --route-corpus corpus.json` exits non-zero when a decision
moved, so a prompt edit is falsifiable in CI rather than a matter of taste.

### Shadow requests — `done`

A sampled share of requests goes to a second provider; one answer is returned,
both are logged, and the admin interface diffs them. Sampling is a counter
rather than a random draw, so the observed rate is exact and a replay is
reproducible.

Sampled requests are buffered rather than streamed, because a diff needs both
complete answers. That is the reason to keep the rate low, and it is stated
next to the setting.

### Triage payload trimming — `done`

The whole client conversation was copied into the triage request, so triage
input grew with conversation length. The copy now keeps every system message
and the newest window of turns, trimmed from the front until it starts on a
user turn so a tool result never arrives without the assistant call that
produced it.

Open: the window is a message count. Trimming by token budget would track the
actual cost more closely, at the price of a tokenizer the proxy does not have.
