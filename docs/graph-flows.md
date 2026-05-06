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

## `graph:` -- state graph

```yaml
version: "1"
name: implement-issue
initial: design
graph:
  design:
    role: architect
    task: ...
    next:
      - implement: "plan complete"
      - aborted: "missing context, cannot plan"
  implement:
    role: developer
    task: ...
    next:
      - review: "all design items implemented"
      - design: "design flaw found, need to revisit"
      - aborted: "cannot proceed safely"
  done:
    final: "Happy-path exit -- implementation reviewed and PR opened."
  aborted:
    final: "Early exit because a step could not proceed safely."
```

A graph flow is a state machine. The runtime starts at `initial:`,
shows the agent the `next:` targets of the current state, and the
agent replies with `{"transition": "<target-state>", "reason": "..."}`.
The runtime jumps to the target state and repeats. The run terminates
at a state with `final:`, or aborts after the configured `max_steps`
budget (currently 30) to bound runaway loops.

### State shapes

**Agent state** -- has `role:`, `task:`, and `next:`:

```yaml
design:
  role: architect
  task: |
    Read the issue, produce a plan.
  next:
    - implement: "plan complete"
    - aborted: "cannot plan"
```

**Shell state** -- has `run:` and `next:` with `pass`/`fail` reasons:

```yaml
verify:
  run: just lint && just test
  next:
    - create-pr: pass
    - implement: fail
```

**Final state** -- has `final: "<description>"`:

```yaml
done:
  final: "Happy-path exit."
```

**Human state** -- has `human: true` (schema-accepted, not yet
runtime-supported):

```yaml
operator:
  human: true
  next:
    - middle: "Operator unblocks."
    - aborted: "Operator aborts."
```

### `next:` entries

Each entry in a state's `next:` list maps a target state to an
optional reason. Supported formats:

```yaml
next:
  - target: "reason"            # single reason
  - target: ["reason1", "r2"]   # list of reasons (OR-combined)
  - target: |                   # multiline reason
      A longer explanation
      spanning multiple lines.
```

For shell states, `pass` and `fail` are reserved reason words.

### `final:` description

The `final:` field carries a non-empty description string documenting
the state's intent. `kuro validate` warns when a final state has an
empty description -- audit consumers need to tell `done` apart from
`aborted`.

Use a graph flow when:

- An agent's verdict needs more than two outcomes (e.g. reviewer wants
  to route back to `design` for design issues vs `implement` for
  code-level fixes).
- The same state is visited more than once per run (rework loops,
  retries with context).
- You want the agent to *name* the next step instead of the user
  manually retriggering a different flow.

## Tradeoffs

| | Linear `flow:` | Graph `graph:` |
|--|--|--|
| Run shape | Fixed | Variable |
| Branching | No | Yes (select targets) |
| Loops | No | Yes (with `max_steps` cap) |
| Agent picks next step | No | Yes |
| Manifest readability | High | Medium (transition log) |
| Validator | DAG cycles, role bindings | Reachability + dead-ends |

Pick the smaller hammer. If a linear flow gets the job done, stay
linear. Reach for `graph:` when the user has been manually deciding
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
graph:
  design:
    role: architect
    task_file: prompts/design.md          # per-state
    next:
      - implement: "plan complete"
      - aborted: "cannot plan"
  implement:
    role: developer
    task_file: prompts/implement.md
    next:
      - done: "all items done"
      - design: "design flaw"
  done:
    final: "Happy-path exit."
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

## Deterministic shell gates: `run:`

A graph state can run a shell command instead of an agent. The exit
code routes the next transition: `0` follows the `pass` entry,
non-zero follows the `fail` entry. No LLM call, no JSON parsing --
the shell is the source of truth for "does it compile, do tests pass".

```yaml
graph:
  review:
    role: reviewer
    task: ...
    next:
      - verify: "all criteria met"
      - implement: "code-level changes needed"

  verify:
    run: just lint && just test
    next:
      - create-pr: pass
      - implement: fail

  create-pr:
    role: developer
    task: ...
    next:
      - done: "PR opened"
      - aborted: "PR creation failed"
```

Use a shell state when the question is mechanical and binary:

- "Does the code compile and do the tests pass?" -- `just lint && just test`
- "Is the workspace clean?" -- `git diff --quiet`
- "Does the seed file match the project copy?" -- `diff -q a b`

Don't use a shell state for judgement calls (code quality, design
review). Those need an agent and a verdict in `{transition, reason}`
JSON.

### Shell-state schema

| Field | Required | Notes |
|--|--|--|
| `run:` | yes | The shell command, run via `sh -c`. |
| `next:` | yes | Exactly 2 entries: one `pass`, one `fail`. |
| `role:`, `task:`, `task_file:` | rejected | Shell states have no agent. |

Self-loops on shell states are rejected: a shell command cannot
recover from its own non-zero exit. Route to a different state
(typically the implementer) to fix the failure.

### What the next state sees

The shell state writes a self-describing artifact to the run
directory under `<run>/steps/NN-<id>.txt`:

```
$ just lint && just test
exit code: 1
--- stdout ---
running 42 tests
test foo::bar ... FAILED
--- stderr ---
thread 'main' panicked at ...
```

The same body is injected into the next state's `prior_context`, so
an implementer routed back from a failing `verify` sees the actual
test failure instead of a paraphrased restatement. Stdout is also
streamed live to the artifact file during execution -- a long-running
`just test` is `tail -f`-able while it runs.

### Variable substitution

`{{vars.X}}` substitution applies to `run:` the same way it does
to `task:` strings, so a flow can parameterize the gate:

```yaml
verify:
  run: "cargo test --test {{vars.suite}}"
  next:
    - pr: pass
    - implement: fail
```

`{{roles.X}}` does not apply -- a shell command does not address an
agent, so role substitution would have no useful target.

### Loop guards

A shell state counts against the same per-state visit cap as any
other state (`DEFAULT_MAX_VISITS_PER_STATE`, currently 5). A flow
that ping-pongs `verify <-> implement` because the implementer can't
get the tests green hits the cap after a few rounds and aborts loud
rather than burning through the full `DEFAULT_MAX_STEPS` budget.

## Role-bound names: `{{roles.<role>}}`

Prompts often need to reference *another* role's agent by name --
"Levi just produced a design plan, Kai will implement it" reads
better than "the team architect produced something the developer
will pick up". Hard-coding the agent name in the flow file works
until a project remaps the role. Use the `roles` namespace instead:

```yaml
graph:
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
