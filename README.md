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
edit YAML: add agents, wire flows, reference shared seed libraries.

## How it works

A **flow** is a process -- "design, implement, review, ship". **Agents** are
the people who execute its steps. **Rules** are shared knowledge injected into
agent prompts. Everything lives as files in your repo:

```
.kuro/
  agents/
    Developer.yaml     # implements the tasks flows hand to it
    Reviewer.yaml      # reviews the developer's changes
  flows/
    hello.yaml         # starter flow: kuro run hello
  rules/
    project-conventions.md  # shared knowledge, injected into prompts
  config.yaml          # role bindings, defaults, seed cascade
```

This is exactly what `kuro init` scaffolds; from there you add agents (an
architect, a facilitator, ...), wire multi-step flows and layer in shared
seed libraries.

Flows are linear sequences or full state machines (`graph:`): an agent's
verdict routes to different states, rework loops revisit earlier states, and
shell states gate progress on real commands like `just lint && just test`.
See [docs/graph-flows.md](docs/graph-flows.md).

Each run writes a persistent, auditable **stack** to
`~/.kuro/stacks/<project>/<run-id>/` -- prompts, responses, and a manifest
pinning what ran. Earlier step results are injected as context into later
steps.

## Sharing seeds across repositories

A **seed** is a directory of agents, rules and flows that a project layers
into its cascade via `seeds:` in `.kuro/config.yaml` (earlier entries win).
Remote seed resolution (`repo:` / `ref:`) is parsed but deliberately
deferred -- see
[docs/decisions/0009-version-pinning.md](docs/decisions/0009-version-pinning.md).
Until it ships, the supported way to consume a shared seed library is a
repository-relative checkout that git pins to an exact commit. The
reference seed library maintained alongside kuromaku lives at
[github.com/nestrai/seeds](https://github.com/nestrai/seeds); the
`your-org/kuromaku-seeds` examples below apply to it unchanged.

### Recommended: commit-pinned Git submodule

Add the seed library as a submodule inside the consuming repository:

```bash
git submodule add https://github.com/your-org/kuromaku-seeds vendor/kuromaku-seeds
```

That produces this layout:

```text
project/
  .kuro/
    config.yaml
  vendor/
    kuromaku-seeds/   # Git submodule pinned by the parent repository
```

and the cascade references it with paths relative to the project root:

```yaml
version: "1"
seeds:
  - path: .kuro/
  - path: vendor/kuromaku-seeds/coding/rust/
  - path: vendor/kuromaku-seeds/github/
  - path: vendor/kuromaku-seeds/coding/common/
```

The parent repository's gitlink records the exact seed commit, so every
clone of the project resolves the identical cascade -- no maintainer-local
paths, nothing outside the repository. kuro resolves from the invocation
directory (it does not search parent directories); run it from the project
root for reproducibility.

### Cloning and recovery

```bash
git clone --recurse-submodules https://github.com/your-org/project
# in an existing clone where vendor/kuromaku-seeds/ is empty:
git submodule update --init --recursive
```

If the submodule is not initialized, kuro fails loudly at config load:

```text
seed path "vendor/kuromaku-seeds/coding/rust/" does not exist
```

That error is the signal to run the recovery command above -- nothing
resolves against a half-present cascade.

### Updating the pin

```bash
git -C vendor/kuromaku-seeds fetch
git -C vendor/kuromaku-seeds checkout <commit-sha>   # or: git submodule update --remote vendor/kuromaku-seeds
git add vendor/kuromaku-seeds
git commit -m "chore(seeds): bump kuromaku-seeds pin"
```

The commit changes only the recorded submodule SHA, so a normal PR diff
shows exactly which seed version the project moves to. Seed updates are
reviewed like any other change.

### Alternative: pinned plain checkout

If submodules do not fit your workflow, a plain clone at the same
repository-relative location works with the identical cascade, as long as
it is checked out at an **immutable commit hash**:

```bash
git clone https://github.com/your-org/kuromaku-seeds vendor/kuromaku-seeds
git -C vendor/kuromaku-seeds checkout <commit-sha>
```

Record that hash somewhere reviewable (a lock note, a setup script). A tag
is acceptable only if you verified and recorded the commit it points to:
tags are mutable references, so a bare tag name does **not** give the same
guarantee as a commit hash.

### Why not a branch

A branch checkout tracks a moving target -- two clones made at different
times resolve different cascades, which breaks reproducibility. Pin a
commit. The rationale (and why a remote resolver is deferred rather than
built now) lives in
[docs/decisions/0009-version-pinning.md](docs/decisions/0009-version-pinning.md).

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
`~/.kuro/stacks/<project>/<run-id>/`, plain files you can inspect. Erase a
project's data with `kuro stack purge <project>` (`--dry-run` to preview) --
see [docs/decisions/0007-stack-purge.md](docs/decisions/0007-stack-purge.md)
for the erasure semantics.

## Legacy paths

kuromaku was briefly named koto; two legacy layouts remain supported so
existing setups keep working, and both are deliberate rather than
accidental:

- **Project config**: `.kuro/config.yaml` is canonical. A `.koto/config.yaml`
  directory or a repo-root `koto.yaml` still loads with a deprecation
  warning (`mv .koto .kuro` migrates in place).
- **Run history**: new runs write under `~/.kuro/stacks/`. Runs recorded
  before the rename stay readable at `~/.koto/stacks/` -- stack-touching
  commands print a one-line notice while that directory holds data, and
  `kuro stack purge` reaches projects under either root. Migrate manually
  per project with `mkdir -p ~/.kuro/stacks/<project> && mv
  ~/.koto/stacks/<project>/* ~/.kuro/stacks/<project>/`; this preserves both
  histories if a project exists under both roots. kuromaku never moves your
  data on its own.

## Status

Under active development. Working today: graph flows with per-state agents,
human-in-the-loop pause/resume, `kuro init`, context injection, template
variables, persistent auditable stacks. Roadmap lives in the
[issues](https://github.com/nestrai/kuromaku/issues).
