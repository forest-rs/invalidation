# ADR-0001: Out-of-scope dependents of targeted drains

## Status

Accepted

## Context

A targeted affected drain (`DrainBuilder::within_keys` or
`DrainBuilder::within_dependencies_of`) takes every invalidated root inside its
scope and expands dependents only inside it. Under `LazyPolicy`, dependents are
marked only when a drain expands them. A key outside the scope that depends on
a drained key is therefore never marked: its pending work is silently lost.

The shape is common in retained graphs. `a` and `b` both read `x`, and `c`
reads `a`. A drain scoped to `c` takes `x`, `a` and `c`; `b` is never
recomputed. `execution_graph` hit this in targeted `run_node` calls and first
worked around it locally.

## Decision

1. `DrainBuilder::retain_out_of_scope(&mut Vec<(K, K)>)` is an explicit opt-in.
   The default stays unchanged, so existing callers keep their behavior.
2. With the option, every **direct** dependent of a drained key that lies
   outside the scope is marked invalidated in the drain's channel. Lazy
   expansion from those marks covers their own dependents on a later affected
   drain, so marking direct dependents is sufficient.
3. Each boundary edge is appended to the caller's vector as
   `(dependent, drained key)`. Every edge is reported, not one per dependent,
   so callers choose how to attribute causes (for example the drained key that
   sorts first). A deterministic drain appends its pairs sorted; otherwise the
   order is unspecified. Existing contents of the vector are kept.
4. The option has no effect on untargeted drains, which leave nothing outside
   their scope, or on invalidated-only drains, which never take responsibility
   for dependents.

## Consequences

- Targeted drains can no longer lose work silently when callers opt in, and
  callers can explain retained work from its real root.
- Retained marks are ordinary invalidated keys: the next drain treats them as
  roots. Callers that trace cause paths use the reported pairs to reconnect
  them to their origin.
- Marking is idempotent; with `EagerPolicy` the dependents are already
  invalidated and are simply reported again.
- Retained keys are marked in the drained channel only, bypassing the
  tracker's channel cascades, and are not reported to a trace recorder. The
  reported pairs are the only record of why they were retained.
