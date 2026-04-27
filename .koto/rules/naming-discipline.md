# Naming Discipline

## Every name is a design decision

Names are not labels you slap on after the design is done. They are the design. A bad name means you haven't understood what the thing represents.

### Principles

- **Specific over generic.** "sources" says nothing. "seeds" says exactly what it is -- reusable building blocks that grow into something. "paths", "config", "settings", "options", "data" are placeholders, not names. If your name could apply to anything, it describes nothing.

- **Warm over corporate.** "registry", "hub", "marketplace", "platform" are interchangeable buzzwords. Prefer organic, grounded words that evoke what they represent. "seeds" won over 20+ alternatives because it's warm, specific, and metaphorically accurate.

- **Domain language over tech jargon.** Use the words your users already use. If the team says "seeds", the config key is `seeds`, not `sources` or `resources` or `dependencies`. Consistency between conversation and config reduces cognitive load.

- **Self-explanatory over documented.** A field name that requires reading the docs has failed. "forge" is better than "vcs_platform" because it's what developers actually call GitHub/GitLab. "tickets" is better than "issue_tracker_backend" because everyone knows what tickets are.

- **Test: would a first-time reader understand this?** Read the name without any surrounding context. If it could mean three different things, it's too generic. If it requires domain knowledge to parse, add a comment -- but first, try a better name.

### Process for naming

1. Write down what the thing IS, in one sentence, without using the candidate name
2. Extract the key concept from that sentence
3. Generate 5+ candidates
4. Test each against "specific, warm, self-explanatory"
5. If none survive, the concept might need restructuring, not just renaming

### Example: the naming process applied

Task: name the config key for where koto finds shared agents, flows, and rules.

1. What it IS: "Reusable building blocks that teams share and that grow into project-specific configurations"
2. Key concept: reusable things that grow into something bigger
3. Candidates: sources, resources, dependencies, packages, seeds, nursery, roots, garden, starters, ingredients
4. Testing:
   - "sources" -- too generic, could mean anything (data sources, code sources)
   - "dependencies" -- implies version locking and package management, wrong mental model
   - "seeds" -- specific, warm, metaphorically accurate (plant, grow, harvest). Unique to koto.
   - "starters" -- close but implies one-time use, seeds can be reused
5. Winner: "seeds" -- it's the only candidate that passes all three tests and wouldn't be confused with a concept from another tool.

### Names to avoid

| Bad | Why | Better |
|-----|-----|--------|
| paths | Could be anything | seeds, sources (if it means origins) |
| config | Everything is config | (name the specific thing being configured) |
| settings | Same as config | (name the specific thing being set) |
| options | Same | (name the specific choices) |
| data | Meaningless | (name the specific data) |
| backend | Vague | forge, tickets, provider |
| platform | Overloaded | (name the specific platform concern) |
| type | Almost always too generic | kind, variant, format |
