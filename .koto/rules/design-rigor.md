# Design Rigor

## Challenge before you build

Every design task starts with questioning, not implementing. The first idea is a hypothesis, not a solution.

### The process

1. **Challenge the framing.** Is this the right question? "How do we structure X?" might be wrong if the real question is "what problem does X solve for the user?" Reframe before solving.

2. **Research prior art.** Search the web. Not from memory -- actually search. What standards exist? What do similar tools do? A quick search that finds an existing standard beats an elaborate design that reinvents it. If you cite prior art without a URL, you're guessing.

3. **Generate real options.** At least 3 structurally different approaches. Not variations of one idea -- genuinely different trade-off profiles. For each: what breaks? What scales? What's the mental model?

4. **Argue for one.** Pick the best option and name the trade-offs you're accepting. Acknowledge what you're giving up. "No trade-offs" means you haven't thought hard enough.

5. **Consider the surface.** No field, flag, or file exists in isolation. How does it interact with everything else? Would a user be surprised by the result?

### Example: rigorous vs shallow design

SHALLOW: "We need a config key for resource paths. Here's the schema:
```yaml
paths:
  agents: .koto/agents
```
Simple and clean."

Why this fails the process: No framing challenge (is 'paths' the right concept?). No prior art (how do Cargo, Terraform, docker-compose handle this?). One option presented as if alternatives don't exist. No trade-offs named. A junior produced this in 5 minutes.

RIGOROUS: "Before designing this, I need to challenge the framing: is this about paths (filesystem locations) or about sources (where resources come from, including remote)? If it's sources, a flat path mapping is the wrong abstraction entirely. Let me check how Cargo.toml handles multi-source dependencies and how Terraform's provider blocks work before proposing options."

Why this is better: challenges the concept before solving it. Recognizes that the framing shapes the solution space. Will research before proposing.

### Anti-patterns

- **Single-option proposals**: Presenting one structure without alternatives is junior work. Senior work means: "here are 3 options, here is why I recommend B."
- **Design-by-committee without friction**: If everyone agrees immediately, the design wasn't challenged. Unanimous consensus on first round is a red flag.
- **Prior art from memory**: "Ansible does it this way" without checking is unreliable. Models hallucinate tool features. Verify before citing.
- **Solving the stated problem**: Sometimes the stated problem is a symptom. Ask "why?" until you reach the actual constraint.
