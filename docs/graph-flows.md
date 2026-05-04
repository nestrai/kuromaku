# Linear flows vs graph flows

kuromaku supports two flow shapes. Both live as YAML in a flows
directory; the binary picks the runtime based on the top-level key.

## `flow:` -- linear

```yaml
version: "1"
name: implement-issue
flow:
  implement:
    role: developer
    task: ...
  review:
    role: reviewer
    input: [implement]
    task: ...
```

A linear flow is a DAG of steps. Each step runs once, the order is
fixed by `input:` dependencies, and each step's output feeds the next.
There is no notion of branching, retry, or loop. PASS/FAIL is encoded
in the agent's text output and consumed by the next step's prompt.

Use a linear flow when:

- The pipeline is fixed -- every run hits every step in the same order.
- A FAIL outcome means "stop and tell the user", not "retry differently".
- You want predictable run shape and simple manifests.

## `states:` -- graph

```yaml
version: "1"
name: implement-issue
initial: design
states:
  design:
    role: architect
    task: ...
    edges:
      design_complete: { to: implement, description: ... }
      blocked:         { to: aborted,   description: ... }
  implement:
    role: developer
    task: ...
    edges:
      implementation_complete: { to: review,  description: ... }
      design_problem:          { to: design,  description: ... }
      blocked:                 { to: aborted, description: ... }
  done:
    kind: final
    description: Happy-path exit -- implementation reviewed and PR opened.
  aborted:
    kind: final
    description: Early exit because a step could not proceed safely.
```

### `description:` on states

States accept an optional `description:` field. It is free-form text
that documents *intent* -- what the state means in the flow, not what
the agent should do (that is `task:`).

`description:` is optional everywhere, but `kuro validate` warns when a
terminal state (`kind: final` or `kind: human`) omits it. Audit
consumers (#257 records the resolved `final_state:` in the manifest)
and human operators have to tell `done` apart from `aborted` without
guessing from the name -- the warning nudges flow authors to make that
intent visible. Non-terminal states do not warn; descriptions there
are still encouraged for graphs that grow past five or six nodes, but
the validator stays quiet to avoid flooding existing flows.

A future Mermaid exporter renders the description as the node label.

A graph flow is a state machine. The runtime starts at `initial:`,
shows the agent the outgoing edges of the current state, and the agent
replies with `{"transition": "<edge-name>", "reason": "..."}`. The
runtime jumps to the target state and repeats. The run terminates at a
state with `kind: final`, or aborts after the configured `max_steps`
budget (currently 30) to bound runaway loops.

Use a graph flow when:

- An agent's verdict needs more than two outcomes (e.g. reviewer wants
  to route back to `design` for design issues vs `implement` for
  code-level fixes).
- The same state is visited more than once per run (rework loops,
  retries with context).
- You want the agent to *name* the next step instead of the user
  manually retriggering a different flow.

## Tradeoffs

| | Linear `flow:` | Graph `states:` |
|--|--|--|
| Run shape | Fixed | Variable |
| Branching | No | Yes (named edges) |
| Loops | No | Yes (with `max_steps` cap) |
| Agent picks next step | No | Yes |
| Manifest readability | High | Medium (edge log) |
| Validator | DAG cycles, role bindings | Reachability + dead-ends |

Pick the smaller hammer. If a linear flow gets the job done, stay
linear. Reach for `states:` when the user has been manually deciding
between two follow-up flows after each run -- that is the case the
graph shape was added for.

## See also

- `seeds/rust/flows/implement-issue.yaml` -- the canonical example
  shipped with the binary.
- `docs/decisions/0006-event-state-machines/` -- the decision record
  motivating graph flows.
- `kuro validate <flow>` -- the same command validates both shapes;
  graph flows additionally get reachability and dead-end checks.
