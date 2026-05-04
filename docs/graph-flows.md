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

## Splitting prompts into files

Both flow shapes accept `prompt_file:` (top-level) and `task_file:`
(per state on graph flows, per step on linear flows). The path is
resolved relative to the directory of the flow YAML, so a graph that
lives at `.kuro/flows/implement-issue.yaml` reads
`.kuro/flows/prompts/design.md` for `task_file: prompts/design.md`.

```yaml
# .kuro/flows/implement-issue.yaml
version: "1"
name: implement-issue
prompt_file: prompts/implement-issue.md   # top-level
initial: design
states:
  design:
    role: architect
    task_file: prompts/design.md          # per-state
    edges:
      design_complete: { to: implement, description: ... }
  implement:
    role: developer
    task_file: prompts/implement.md
    edges:
      implementation_complete: { to: done, description: ... }
  done:
    kind: final
    description: Happy-path exit.
```

```
.kuro/flows/
  implement-issue.yaml
  prompts/
    implement-issue.md
    design.md
    implement.md
```

Resolution rules:

- **Sibling paths only.** The reference must resolve to a file under
  the flow YAML's directory. Absolute paths and any reference whose
  components include `..` are rejected before any I/O. Symlinks that
  point outside the flow directory are rejected after canonicalize.
- **Mutual exclusion.** Setting both `task:` and `task_file:` on the
  same state (or both `prompt:` and `prompt_file:` at the top level)
  is a validation error -- pick one. The error names the offending
  flow path, state/step ID, and the field pair.
- **Variable substitution still applies.** `{{vars.X}}` placeholders
  inside a loaded prompt file get the same substitution pass as
  inline `task:` strings. The file is read first, then substituted.
  `{{roles.X}}` (see below) resolves the same way regardless of
  whether the prompt was inline or loaded from a sibling file.
- **Missing files are validation errors.** `kuro validate` reports
  missing prompt files with the flow path and state/step ID, exits
  non-zero, and writes the error to stderr -- the same channel as
  the graph reachability and dead-end errors.

When to split:

- The inline `task:` is more than a paragraph and pushes the graph
  topology off-screen.
- Two states share the bulk of their prompt -- a shared file plus a
  small inline tail beats copy-pasting the whole prompt twice.
- The prompt is iterated on during a flow's lifetime and a focused
  diff in `prompts/<state>.md` reads better than a diff buried in
  the YAML.

## Role-bound names: `{{roles.<role>}}`

Prompts often need to reference *another* role's agent by name --
"Levi just produced a design plan, Kai will implement it" reads
better than "the team architect produced something the developer
will pick up". Hard-coding the agent name in the flow file works
until a project remaps the role. Use the `roles` namespace instead:

```yaml
states:
  steer_design:
    role: steering
    task: |
      {{roles.architect}} just produced a design plan for issue
      #{{vars.id}}. Decide the next step.
```

`{{roles.architect}}` substitutes to whatever agent the cascade
binds to the `architect` role at run time:

1. CLI `--role architect=<agent>` (highest precedence)
2. Project config `roles.architect.agent` in `.kuro/config.yaml`
3. Flow-file `roles:` default (linear flows only -- graph flows
   declare `role:` per state, no flow-file default)

If no layer binds the role, the run aborts before any agent
spawns with a clear error: `unknown role 'architect' referenced
in state 'steer_design'`. Substitution happens after `{{vars.X}}`
in both linear and graph flows, and applies to the top-level
`prompt:` and every per-step / per-state `task:`.

Out of scope (do not rely on these working):

- `{{roles.<role>.model}}` and other agent metadata -- only the
  agent name is exposed.
- `{{agents.<id>...}}` -- a different namespace, not implemented.
- Conditional / loop templating -- there is no Jinja-style logic.

## See also

- `seeds/rust/flows/implement-issue.yaml` -- the canonical example
  shipped with the binary.
- `docs/decisions/0006-event-state-machines/` -- the decision record
  motivating graph flows.
- `kuro validate <flow>` -- the same command validates both shapes;
  graph flows additionally get reachability and dead-end checks.
