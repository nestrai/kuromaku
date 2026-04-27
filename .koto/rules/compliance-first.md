# Compliance-First Design

## Auditability is not a feature -- it's a constraint

Every system that involves AI decisions in a professional context will eventually face compliance requirements. EU AI Act, internal audit, client due diligence. Design for this from day one, not as a retrofit.

### Core principle: versioniert, diffbar, auditierbar

Every configuration, every agent selection, every decision in the system must be:

- **Versioniert**: Tracked in version control. If it's not in git, it doesn't exist for audit purposes.
- **Diffbar**: Changes are visible in diffs. A config change between two runs must be reviewable by a human.
- **Auditierbar**: The system can answer "what ran, with what config, producing what output?" for any past execution.

### Design rules

1. **Config file over env vars.** Environment variables are invisible, unversioned, and differ between machines. koto.yaml is the single source of truth. CLI overrides are logged in audit output.

2. **Resolution audit trail.** Every run must show which agent was loaded from which source, which flow version was used, which rules were applied. "It used Sage" is not enough -- "Sage v1.2 from koto-ai/seeds@abc123, resolved via koto.yaml roles.developer" is.

3. **Reproducibility.** Given the same koto.yaml and the same seed refs, the same configuration must be assembled. No implicit defaults that change between versions. No "it worked on my machine" because an env var was set differently.

4. **Explicit over implicit.** Defaults are documented. Overrides are logged. Hidden behavior is a compliance risk. If something happens without the user configuring it, it must be visible in the audit output.

5. **Declarative over imperative.** The config file declares the desired state. The runtime resolves it deterministically. No procedural logic in config, no side effects from field ordering.

### What this means in practice

- No `KOTO_MODEL=gpt-4` overriding koto.yaml silently. If CLI flags override config, the resolution output shows it.
- Pinned refs (`ref: v0.1`) in seed sources, not floating tags or `latest`.
- Agent definitions include version metadata so the audit trail is complete.
- Every flow run produces a resolution summary: what came from where, what was overridden, what defaults were used.

### The compliance test

Before approving any design that touches configuration or agent selection, ask:
1. Can an auditor reconstruct exactly what ran and why, six months from now?
2. Is every override visible in the output, or are some hidden behind env vars or implicit defaults?
3. Would the same config produce the same result on a different machine?
If any answer is no, the design has a compliance gap.
