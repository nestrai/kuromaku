# seeds/common -- cross-cutting seed bucket

This bucket contributes personas that are not specific to any programming
language or hosting provider. It sits at the lowest priority in the
tracked cascade (see `.kuro/config.yaml`) so higher-priority seeds can
shadow individual agents without removing the fallback defaults.

## Agents

- **Mika** -- Facilitator. Moderates multi-agent discussions, surfaces
  dissent, and drives toward a recorded decision. Unanimous agreement
  without friction is treated as a failure signal. Output: decision,
  trade-offs, dissent (verbatim), next steps.
- **Minion** -- Data fetcher. Runs read-only shell commands (gh, git,
  curl) and outputs raw results without analysis or filtering. Does not
  substitute a guess when a command fails -- reports the error verbatim.

## Cross-links

- `.kuro/SEED.md` -- top-level seed inventory and cascade description
- `seeds/rust/SEED.md` -- Rust-stack personas and PR lifecycle flows
