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

### Cost and spend views — `idea`

Surface for the spend meter above, once provider cost metadata exists.

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
