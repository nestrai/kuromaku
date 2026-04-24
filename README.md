# koto

Reproducible AI agent teams, defined in YAML, versioned in your repo.

Define your team once, run it anywhere. No Python, no framework lock-in, no glue code.

```
koto up review-pr id=67
```

## What it does

koto orchestrates AI agent teams through declarative YAML configurations. A **flow** is a process -- a reusable sequence of steps like "fetch, implement, review, ship". **Agents** are the people who execute those steps. **Rules** are shared knowledge injected into agent prompts. The same flow can run with different agent teams depending on the task.

```
.koto/
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
mkdir -p .koto/agents .koto/flows .koto/rules

# Define an agent
cat > .koto/agents/Reviewer.yaml << 'EOF'
name: Reviewer
title: code reviewer
role: |
  You are a code reviewer. Review changes for correctness, edge cases,
  test coverage, and adherence to project conventions.
rules: [go-developer]
EOF

# Define a flow with template variables
cat > .koto/flows/review-pr.yaml << 'EOF'
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
koto up review-pr id=67
```

## Key concepts

**Agents** are YAML files in `.koto/agents/`. Each agent has a name, title, role (system prompt), and references to shared rules. Agents can use different models and backends.

**Flows** are YAML files in `.koto/flows/`. A flow defines steps as a map -- keys are step names, values configure which agent runs and what context they receive from prior steps.

**Rules** are Markdown files in `.koto/rules/`. Multiple agents can reference the same rules. Rules are composed into the system prompt at runtime: Guide > Rules > Skills > Role.

**Stack** is where outputs land. Each step writes its result to `~/.koto/stacks/<project>/`. Results from earlier steps are injected as context into later steps.

**Template variables** allow flows to define reusable prompts with `{{key}}` placeholders, filled via `key=value` CLI arguments.

## How it compares

koto takes a fundamentally different approach from Python-based agent frameworks.

|  | koto | CrewAI | LangGraph | AutoGen |
|---|---|---|---|---|
| Configuration | YAML only | Python + YAML | Python only | Python only |
| User code required | No | Yes | Yes | Yes |
| Agent definitions | Standalone files, reusable across flows | Embedded in Python package | Python objects | Python objects |
| Rule composition | `rules: [rust-dev, cli-ux]` -- Markdown files composed per agent | Inline text in `backstory` field | N/A | N/A |
| LLM backend per agent | Claude CLI, Ollama, API -- mix freely | Via LiteLLM (per agent) | Via LangChain (per node) | Per agent |
| Execution targets | Local, SSH, Kubernetes | Local only | Local only | Local only |
| State persistence | Structured stack on disk | In-memory, session-scoped | External checkpointers | Message history |
| Reproducibility | `git diff` on YAML | Depends on code discipline | Depends on code discipline | Depends on code discipline |
| Setup | Single binary | `pip install` + Python env | `pip install` + Python env | `pip install` + Python env |

### What this means in practice

**No framework lock-in.** Your team definition is YAML. Tomorrow a better LLM CLI appears -- you change one line in the agent file. No Python class hierarchies to refactor, no decorators to update, no dependency chains to untangle.

**Repo-native by design.** `.koto/` lives next to your code. Agents, rules, and flows are versioned, diffable, and reviewable in PRs. Anyone on the team can read the YAML and understand what the AI agents do -- no Python knowledge required.

**Composable knowledge.** Rules are Markdown files that multiple agents share. A `go-developer.md` rule works for your architect, your developer, and your reviewer. Change it once, every agent picks it up. In other frameworks, you duplicate instructions across agent backstories.

**Backend-agnostic.** Run your architect on Claude API for complex reasoning, your developer on a local Ollama model for speed, your reviewer on Claude CLI for tool access -- all in the same flow. The orchestration layer does not care which LLM serves the response.

## Build

```bash
# With Nix
nix develop
just build

# With Cargo
cargo build --release
```

## Status

Early development. The core flow engine works: sequential step execution, context injection between steps, template variables, persistent stack output. See the [issues](https://github.com/charemma/koto/issues) for the roadmap.
