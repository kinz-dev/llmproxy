# llmproxy Roadmap

Candidate work, grouped by what it delivers. Nothing here is committed; the
ordering inside each group is a rough preference, not a schedule.

Status: `idea` · `designed` · `building` · `done`

## End-User Features

These exploit llmproxy's position: it is the only component that sits across
every agent, every provider, and every session, and it sees complete requests
and responses locally.

### Cross-agent shared project memory — `idea`

Each agent CLI keeps its own history and shares nothing with the others. The
proxy sees all of them. A project-scoped store of prior findings, decisions,
and dead ends, injected into whichever agent is running, gives continuity that
is impossible at any other layer because each vendor sees only its own client.

### Retry-driven escalation — `idea`

Re-asking the same question a different way is a dissatisfaction signal, and it
is visible to the proxy: same conversation, near-identical embedding, no
intervening tool use. Answer first on the cheap route; silently escalate the
retry to a stronger provider. No new setting, no model picker — routing driven
by observed feedback rather than hand-written hints.

### Transparent quota and outage failover — `idea`

Rate-limit exhaustion mid-task is the sharpest failure in agent-assisted work.
The proxy owns every provider credential and can absorb it: exhausted on A,
continue on B, report it in the admin UI rather than to the caller. The same
machinery covers network loss by falling back to a local provider.

### Second opinion on consequential requests — `idea`

Opt in per target (for example on a route hinted at design decisions): ask two
providers, present both answers, or have a third reconcile them. Agreement is
signal; divergence is a stronger signal, and nothing surfaces it today.

### Personal model leaderboard — `idea`

Public benchmarks rank models on generic tasks. The proxy observes the request,
the response, and the aftermath — accepted, reverted, immediately re-asked — on
real work in a real repository. That is free implicit labelling, and it yields a
per-project ranking plus proposed hints derived from observed clusters rather
than guessed at by hand.

### Spend meter and cost insight — `idea`

Live per-project spend, and the insight built on it: what share went to an
expensive route for work a cheaper route answers correctly, with a one-click
routing change. Depends on per-provider cost metadata in provider config.

## Admin UI

### Per-provider flow ribbons — `building`

The overview flow diagram is hardcoded to two paths, local and external, with
literal SVG coordinates. `served_by_provider` already carries count and latency
percentiles per provider and is already served by the stats API, so this is a UI
change only.

Design decisions, recorded because they are easy to get wrong:

- **Colour stays bound to locality, not provider identity.** The whole UI
  encodes local/external through `--local` and `--external`; per-provider hues
  would break that and require validating a fresh categorical palette. Identity
  is carried by stable position and a direct label, so it is never colour-alone.
- **Ribbon order is stable, never sorted by volume.** Sorting by request count
  makes providers swap position on every poll. Group by locality, then sort
  alphabetically; only thickness varies.
- **Cap the fan-out.** Vertical room allows roughly six ribbons before labels
  collide. Beyond that, fold the tail into one aggregate ribbon.
- **Update keyed, not wholesale.** The drift animation restarts if the nodes are
  rebuilt each poll, which visibly stutters. Reuse each provider's node.

### Per-model spend analytics — `idea`

Price the existing per-model token counters so the admin UI answers "what is
this costing me", broken down by model rather than in aggregate.

Most of the data is already collected. `ModelUsage` keys requests, input tokens,
output tokens, and total time by (provider, model); the stats API serves it as
`model_usage`, and the UI already renders a Model usage table. What is missing
is a price and the multiplication.

- **Price metadata.** No cost information exists in config today. Rates are
  per-provider *and* per-model, and differ between input and output, so this
  wants a table keyed by model under each provider rather than a single number.
  Prices change, so treat the config as the source of truth and never hardcode.
- **Cached tokens are not recorded.** `cached_tokens` is translated through the
  protocol layer but never reaches stats. Cache reads bill at a steep discount
  and cache writes at a premium, so a naive input-token multiplication
  overstates spend badly on agent traffic, which is cache-heavy by nature.
  Recording them is a prerequisite for a number worth trusting.
- **Estimated counts must be labelled.** `ModelUsage.estimated_requests` counts
  requests whose tokens llmproxy estimated rather than read from the provider
  response. Spend derived from those is an approximation and the UI should say
  so rather than presenting a precise-looking figure.
- **Counters do not survive a restart.** Stats are in-memory with a reset
  action and no serialisation, so totals die with the daemon. Per-session cost
  works today; "what did I spend this month" needs durable storage first, and
  that is the larger half of this feature.
- **Subscription providers are not per-token.** A provider using borrowed CLI
  OAuth bills against a plan, not per token. Showing a dollar figure there is
  actively misleading; those rows should read as included in the subscription
  while still reporting token volume.

Pairs with the spend meter under End-User Features, which is the same data
aggregated per project rather than per model.

## Routing

### Project-scoped routing overlay — `designed`

Per-project semantic route tables, so a project can send development work to one
provider, front-end work to another, and everything else to a third, without any
project identity reaching the triage model.

- A project file may define `[integrated.targets.*]` and `[model_aliases]`
  only. Every provider it names must already exist in the user-level config.
  Providers, base URLs, and credentials stay user-level: a project file lives in
  a repository, and cloning an untrusted repository must not be able to define
  where prompts are sent.
- Project targets **merge** with user-level targets; the project wins on a name
  collision.
- A `default = true` field marks the catch-all target. Validation rejects more
  than one after merge, and `ROUTE_DEFAULT` resolves to it in code instead of
  falling through to first-upstream-model discovery.
- The scope reaches the daemon through the launcher session token, so one daemon
  continues to serve every project. The token is unsigned, so the daemon
  re-validates the scope rather than trusting it.

Open: hint overlap between a broad route and a narrower one has no tie-break
today. Disjoint hint wording works now; an explicit `priority` field on targets
is the structural fix if that wording grows unwieldy.

### Sticky routing never expires during an active session — `idea`

The sticky TTL is a sliding window, not an absolute one, so it does not bound
how long a route stays pinned in real use.

`conversation_key` prefers a session or thread header, and agent CLIs send one
on every turn, so the key is stable for an entire session rather than per
message. `execute_integrated_redirect` then re-inserts the sticky entry after
every successful redirect, refreshing the TTL. The result is that the first
message of a session chooses the route, and the choice survives for as long as
the user keeps working — no value of `sticky_ttl_secs` changes this, because the
TTL only ever governs idle gaps.

This is the right behaviour for its original purpose, which is keeping one
conversation coherent on one provider. It becomes wrong when routes differ in
cost or capability: a session that opens with a trivial question is pinned to
the cheap route even as the work turns into deep design, which defeats most of
the value of splitting routes by complexity in the first place.

Candidate directions, none of which require in-band verbs:

- **Absolute rather than sliding TTL.** Keep the insertion timestamp on refresh
  so the route is re-evaluated periodically even under continuous use.
- **Re-triage on material change.** Re-run routing when the request diverges
  enough from the one that set the route; the embedding work under
  Embedding-cached routing would supply the distance measure.
- **Bound by turns, not only by time.** Let a sticky entry serve at most N
  turns before routing is reconsidered.
- **Escalate but never demote.** Allow a later turn to move up to a stronger
  route while refusing to move down, so cost control survives without pinning
  quality to whatever the first message happened to be.

Whichever is chosen, the Decisions view should distinguish a sticky reuse from a
fresh decision so the behaviour is visible rather than inferred.

### In-band routing verbs — `idea`

There is no way to re-trigger triage inside an active session. `conversation_key`
prefers a session or thread header, which agent CLIs send on every turn, so the
key is stable for the whole session; and a successful redirect re-inserts the
sticky entry, refreshing its TTL. An actively worked session therefore never
falls out of stickiness, and the first message of the session decides the route
until the user stops for longer than the TTL. Lowering `sticky_ttl_secs` does
not address this — the TTL only governs idle gaps, never an active session.

Proposal: recognise a small closed set of verb prefixes at the very start of the
newest user message, act on them, and strip them before the request is
forwarded. `triage: <prompt>` would bypass the sticky entry and re-run routing;
a target name would pin that route directly and skip triage altogether, which is
both deterministic and free of triage latency.

In-band is the right channel precisely because agent CLIs give the user no way
to set a per-message header. The response already carries `x-llmproxy-route`,
so the mechanism for reporting what happened exists; only the inbound direction
is missing.

To settle at implementation time:

- **Where detection happens.** Before the sticky lookup in `maybe_intercept`,
  or the sticky entry short-circuits the verb.
- **Stripping both copies.** The prefix must not reach the provider, and the
  local copy must never mutate the external copy, so the strip has to be applied
  deliberately to each rather than falling out of the existing clone.
- **Stale prefixes in history.** Earlier turns keep their prefixes in the
  conversation. Those must be stripped or ignored, or the triage model reads old
  directives as if they were current.
- **Collision safety.** Only at the start of the newest user message, only an
  exact match against the closed verb list, with an escape for the case where
  someone genuinely begins a message that way.
- **Verb vocabulary.** `triage:` to force re-routing, a target name to pin,
  something for "answer locally", and possibly an explicit unstick. Target names
  are already constrained to tool-name-safe characters, so they parse
  unambiguously, but they are verbose; short aliases would need defining.

Related to retry-driven escalation under End-User Features: this is the
explicit, user-driven form of the same need.

### Embedding-cached routing — `idea`

`RoutingDecisionAudit` already records prompt, decision, target, and reason for
every request, then discards it after 24 hours. Embedding the request and
matching against past decisions turns that into a routing cache: triage cost
drops, hit-path latency approaches zero, and identical requests route
identically by construction. Sticky routing generalised from one conversation to
all similar requests. The end state is distillation, with the LLM tier as the
fallback rather than the default.

### Redact and route — `idea`

`reject_sensitive` is binary today: detect and refuse. Substituting placeholders,
routing to a strong external provider, and rehydrating in the response converts
"cannot use a good external model on this code" into "can". Hardest part is
rehydration across streaming and tool-call arguments.

### Speculative triage — `idea`

Triage is a serial round trip ahead of each new conversation. Issuing the triage
call and the presumed-default route concurrently, cancelling the loser, removes
it from the critical path at the cost of some wasted tokens. Buffered
speculation must not reach the conversation.

### Provider failover and circuit breaking — `idea`

`fallback_on_error` is one-dimensional: local to external. Health tracking and
failover between external providers is the missing axis.

## Platform

### Routing regression suite — `idea`

Single-shot route testing exists (`RouteTestMode`). A corpus version — capture
real requests, replay against an edited prompt or target set, diff the
decisions — makes prompt edits falsifiable instead of a matter of taste, and
makes every routing change above safer to attempt.

### Shadow requests — `idea`

Send a sampled share of requests to two providers, return one, log both, and
diff them in the admin UI. Turns provider choice into evidence.

### Triage payload trimming — `idea`

The whole client conversation is copied into the triage request, so triage input
grows with conversation length. Sticky routing means the cost is paid once per
conversation rather than per turn. Trimming the copy to the system prompt plus
recent turns would cut cost and likely improve hint matching by reducing noise.
