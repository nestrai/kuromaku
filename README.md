# kuromaku

Reproducible AI agent teams, defined in YAML, versioned in your repo.

Define your team once, run it anywhere. No Python, no framework lock-in, no glue code.

```
kuro run review-pr id=67
```

## What it does

kuromaku orchestrates AI agent teams through declarative YAML configurations. A **flow** is a process -- a reusable sequence of steps like "fetch, implement, review, ship". **Agents** are the people who execute those steps. **Rules** are shared knowledge injected into agent prompts. The same flow can run with different agent teams depending on the task.

```
.kuro/
  agents/
    Levi.yaml          # software architect
    Noah.yaml          # senior developer
    Bella.yaml         # code reviewer
  flows/
    review-pr.yaml     # architect -> reviewer -> consensus
    development.yaml   # design -> implement -> review
  rules/
    go-developer.md    # shared knowledge, injected into prompts
  Guide.md             # repo-wide context for all agents
```

## Quick start

```bash
# In your project directory
mkdir -p .kuro/agents .kuro/flows .kuro/rules

# Define an agent
cat > .kuro/agents/Reviewer.yaml << 'EOF'
name: Reviewer
title: code reviewer
role: |
  You are a code reviewer. Review changes for correctness, edge cases,
  test coverage, and adherence to project conventions.
rules: [go-developer]
EOF

# Define a flow with template variables
cat > .kuro/flows/review-pr.yaml << 'EOF'
version: "1"
name: review-pr
prompt: |
  Review PR #{{id}} in this repository. Fetch the PR diff
  using the gh CLI. Evaluate the changes thoroughly.

flow:
  review:
    agent: Reviewer
EOF

# Run it
kuro run review-pr id=67
```

## Key concepts

**Agents** are YAML files in `.kuro/agents/`. Each agent has a name, title, role (system prompt), and references to shared rules. Agents can use different models and backends.

**Flows** are YAML files in `.kuro/flows/`. A flow defines steps as a map -- keys are step names, values configure which agent runs and what context they receive from prior steps. Beyond linear sequences, a flow can be a full state machine (`graph:`) where an agent's verdict routes to different states, rework loops revisit earlier states, and shell states gate progress on real commands like `just lint && just test` -- see [docs/graph-flows.md](docs/graph-flows.md).

**Rules** are Markdown files in `.kuro/rules/`. Multiple agents can reference the same rules. Rules are composed into the system prompt at runtime: Guide > Rules > Skills > Role.

**Stack** is where outputs land. Each step writes its result to `~/.koto/stacks/<project>/`. Results from earlier steps are injected as context into later steps.

**Template variables** allow flows to define reusable prompts with `{{key}}` placeholders, filled via `key=value` CLI arguments.

## Human-in-the-loop

A graph state with `human: true` hands control to a person instead of an agent:

```yaml
review:
  agent: Bella
  next:
    - merge: "Changes look good."
    - clarify: "Reviewer needs a human decision."

clarify:
  human: true
  next:
    - implement: "Human requested changes."
    - merge: "Human approved as-is."
```

On an interactive terminal, `kuro run` prompts you inline when the flow reaches
the human state. In an unattended run, the flow pauses cleanly and records the
handoff; you continue it later with:

```bash
kuro resume <run-id> --message "approved, but rename the flag first"
# or pipe the response: echo "go" | kuro resume <run-id>
# or from a file:       kuro resume <run-id> --message-file review-notes.md
```

Your response is injected as context into the next state, so the agents see
exactly what the human decided and why. Because every run is a persistent
stack on disk, a paused flow survives reboots -- resume it tomorrow.

## How it compares

kuromaku takes a fundamentally different approach from Python-based agent frameworks.

|  | kuromaku | CrewAI | LangGraph | AutoGen |
|---|---|---|---|---|
| Configuration | YAML only | Python + YAML | Python only | Python only |
| User code required | No | Yes | Yes | Yes |
| Agent definitions | Standalone files, reusable across flows | Embedded in Python package | Python objects | Python objects |
| Rule composition | `rules: [rust-dev, cli-ux]` -- Markdown files composed per agent | Inline text in `backstory` field | N/A | N/A |
| LLM backend per agent | Claude CLI, Ollama, API -- mix freely | Via LiteLLM (per agent) | Via LangChain (per node) | Per agent |
| Execution targets | Local, SSH, Kubernetes | Local only | Local only | Local only |
| State persistence | Structured stack on disk | In-memory, session-scoped | External checkpointers | Message history |
| Human-in-the-loop | `human: true` states -- inline on TTY, pause + `kuro resume --message` unattended | `human_input` flag (blocking prompt) | `interrupt()` + checkpointer, in code | `UserProxyAgent` |
| Reproducibility | `git diff` on YAML | Depends on code discipline | Depends on code discipline | Depends on code discipline |
| Setup | Single binary | `pip install` + Python env | `pip install` + Python env | `pip install` + Python env |

### What this means in practice

**No framework lock-in.** Your team definition is YAML. Tomorrow a better LLM CLI appears -- you change one line in the agent file. No Python class hierarchies to refactor, no decorators to update, no dependency chains to untangle.

**Repo-native by design.** `.kuro/` lives next to your code. Agents, rules, and flows are versioned, diffable, and reviewable in PRs. Anyone on the team can read the YAML and understand what the AI agents do -- no Python knowledge required.

**Composable knowledge.** Rules are Markdown files that multiple agents share. A `go-developer.md` rule works for your architect, your developer, and your reviewer. Change it once, every agent picks it up. In other frameworks, you duplicate instructions across agent backstories.

**Backend-agnostic.** Run your architect on Claude API for complex reasoning, your developer on a local Ollama model for speed, your reviewer on Claude CLI for tool access -- all in the same flow. The orchestration layer does not care which LLM serves the response.

## Privacy

kuromaku stores every run on your local machine -- nothing leaves the host unless you wire it up yourself. Run artifacts (prompts, agent responses, manifests, audit logs) live under:

```
~/.koto/stacks/<project>/<run-id>/
```

`<project>` defaults to the current directory's name; `<run-id>` is the timestamped flow run.

**Inspect.** Browse the directory with the file manager of your choice. Each run dir contains a `manifest.yaml` that pins what was used (flow, rules, skills, agents) plus a `steps/` subdirectory with one Markdown file per step.

**Erase.** Delete a single project's stack data with:

```
kuro stack purge <project>           # asks for confirmation
kuro stack purge <project> --dry-run # preview only, no deletion
kuro stack purge <project> --yes     # skip the prompt (scripted use)
```

This is the GDPR Art. 17 mechanism for kuromaku: when you receive an erasure request for a data subject whose data is contained in a project's stack, `kuro stack purge` removes that project's directory atomically. Erasure is hard-delete; the deletion itself is not recorded by kuromaku (see `docs/decisions/0007-stack-purge.md` for the rationale and how to add a controller-side audit trail if your obligations require one).

## Install

Prebuilt binaries for Linux and macOS (x86_64 and aarch64) are attached to
every [release](https://github.com/nestrai/kuromaku/releases):

```bash
# Grab the latest release for your platform, e.g. Linux x86_64:
curl -fsSL https://github.com/nestrai/kuromaku/releases/latest/download/kuro-x86_64-linux.tar.gz \
  | tar -xz && sudo install -m 755 kuro /usr/local/bin/kuro
```

With Nix, the flake exposes the binary directly:

```bash
nix profile install github:nestrai/kuromaku
# or, without installing:
nix run github:nestrai/kuromaku -- --help
```

Or build from source:

```bash
git clone https://github.com/nestrai/kuromaku && cd kuromaku
cargo build --release   # or: nix develop -c just release
```

## Status

Under active development. The graph flow engine works: state-machine flows with
per-state agents, parallel-by-default stages, human-in-the-loop pause/resume,
context injection between states, template variables, and a persistent,
auditable stack per run. See the [issues](https://github.com/nestrai/kuromaku/issues)
for the roadmap.
