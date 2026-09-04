# kuromaku

Reproducible AI agent teams, defined in YAML, versioned in your repo.

Define your team once, run it anywhere. No Python, no framework lock-in, no glue code.

```
kuro run review-pr id=67
```

## Install

```bash
# Prebuilt binary (Linux/macOS, x86_64/aarch64), e.g. Linux x86_64:
curl -fsSL https://github.com/nestrai/kuromaku/releases/latest/download/kuro-x86_64-linux.tar.gz \
  | tar -xz && sudo install -m 755 kuro /usr/local/bin/kuro

# Nix
nix profile install github:nestrai/kuromaku

# From source
cargo build --release
```

All binaries are on the [releases page](https://github.com/nestrai/kuromaku/releases).

## Quick start

```bash
cd your-project
kuro init            # detects your stack, writes a working .kuro/
kuro run hello       # first flow run, confirms the setup
kuro context         # shows what agents, rules and flows are active
```

`kuro init` scaffolds agents, a rule stub and a starter flow. From there you
edit YAML: add agents, wire flows, reference shared seed libraries. To share
a seed library across repositories, pin it as a commit-pinned Git submodule --
see [docs/seed-pinning.md](docs/seed-pinning.md) for the supported pattern.

## How it works

A **flow** is a process -- "design, implement, review, ship". **Agents** are
the people who execute its steps. **Rules** are shared knowledge injected into
agent prompts. Everything lives as files in your repo:

```
.kuro/
  agents/
    Levi.yaml          # software architect
    Noah.yaml          # senior developer
  flows/
    review-pr.yaml     # architect -> reviewer -> consensus
  rules/
    rust-developer.md  # shared knowledge, injected into prompts
  Guide.md             # repo-wide context for all agents
```

Flows are linear sequences or full state machines (`graph:`): an agent's
verdict routes to different states, rework loops revisit earlier states, and
shell states gate progress on real commands like `just lint && just test`.
See [docs/graph-flows.md](docs/graph-flows.md).

Each run writes a persistent, auditable **stack** to
`~/.koto/stacks/<project>/` -- prompts, responses, and a manifest pinning
what ran. Earlier step results are injected as context into later steps.

## Human-in-the-loop

A graph state with `human: true` hands control to a person:

```yaml
clarify:
  human: true
  next:
    - implement: "Human requested changes."
    - merge: "Human approved as-is."
```

On a terminal, `kuro run` prompts you inline. Unattended, the flow pauses and
resumes later -- your response is injected as context for the next state:

```bash
kuro resume <run-id> --message "approved, but rename the flag first"
```

## How it compares

|  | kuromaku | CrewAI | LangGraph | AutoGen |
|---|---|---|---|---|
| Configuration | YAML only | Python + YAML | Python only | Python only |
| User code required | No | Yes | Yes | Yes |
| Agent definitions | Standalone files, reusable across flows | Embedded in Python package | Python objects | Python objects |
| Rule composition | `rules: [rust-dev, cli-ux]` -- Markdown files composed per agent | Inline text in `backstory` field | N/A | N/A |
| LLM backend per agent | Claude CLI, Ollama, API -- mix freely | Via LiteLLM (per agent) | Via LangChain (per node) | Per agent |
| State persistence | Structured stack on disk | In-memory, session-scoped | External checkpointers | Message history |
| Human-in-the-loop | `human: true` states -- inline on TTY, pause + `kuro resume` unattended | `human_input` flag (blocking prompt) | `interrupt()` + checkpointer, in code | `UserProxyAgent` |
| Reproducibility | `git diff` on YAML | Depends on code discipline | Depends on code discipline | Depends on code discipline |
| Setup | Single binary | `pip install` + Python env | `pip install` + Python env | `pip install` + Python env |

## Privacy

Everything stays on your machine: run artifacts live under
`~/.koto/stacks/<project>/<run-id>/`, plain files you can inspect. Erase a
project's data with `kuro stack purge <project>` (`--dry-run` to preview) --
see [docs/decisions/0007-stack-purge.md](docs/decisions/0007-stack-purge.md)
for the erasure semantics.

## Status

Under active development. Working today: graph flows with per-state agents,
human-in-the-loop pause/resume, `kuro init`, context injection, template
variables, persistent auditable stacks. Roadmap lives in the
[issues](https://github.com/nestrai/kuromaku/issues).
