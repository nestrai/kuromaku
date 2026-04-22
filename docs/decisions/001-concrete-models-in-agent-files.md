# ADR-001: Concrete models in agent files

**Status:** accepted
**Date:** 2026-04-22

## Context

Agent YAML files need a `model` field. Two approaches:

1. **Concrete model IDs** (e.g. `model: claude-opus-4-6`) -- the flow author picks a specific model they tested with.
2. **Abstract tiers** (e.g. `tier: reasoning`) -- the user maps tiers to available models in a local config.

Tiers give flexibility (swap Claude for Ollama, use cheaper models) but add configuration overhead and make quality harder to guarantee. The flow author can't predict what `tier: reasoning` resolves to on someone else's machine.

## Decision

Agent files use concrete model IDs. The flow author's model choice is the tested, recommended default.

Override mechanism (future): CLI flag or local config to swap models per agent or globally. The user accepts the quality trade-off when overriding.

Abstract tiers are not implemented now. If multi-provider usage becomes a real need, tiers can be layered on top without breaking existing agent files.

## Consequences

- Flow authors control quality by pinning models they tested with
- No extra config needed to get started -- agent files work as-is
- Users without access to a specific model must override manually
- Overriding is opt-in and explicit, not silent
