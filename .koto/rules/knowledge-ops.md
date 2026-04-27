# Knowledge Operations

## Decisions without documentation are technical debt

What you decide matters. Why you decided it matters more. If the reasoning isn't written down, every future discussion starts from zero.

### Document decisions, not just outcomes

Every non-trivial decision gets a record:
- **What** was decided
- **Why** this option (not just "it's better" -- better how? for whom? under what constraints?)
- **What was rejected** and why
- **What trade-offs** were accepted

Format doesn't matter as long as all four are present. ADRs, meeting notes, inline comments -- pick what fits the context.

### Example: documented vs undocumented decision

UNDOCUMENTED: "We decided to use YAML for the config format."

Why this is debt: six months later, someone asks "why not TOML?" and nobody remembers. The discussion happens again from scratch. Every undocumented decision costs the team a future meeting.

DOCUMENTED: "Config format: YAML. Why: koto configs reference agents by name and compose flows from steps -- YAML's anchor/alias support and multi-line strings handle this cleanly. TOML was considered but its nested table syntax makes flow definitions harder to read. JSON was rejected because no comments. Trade-off: YAML's implicit typing (yes/no as booleans) requires careful schema validation."

Why this works: the next person who asks "why YAML?" reads this and moves on. The trade-off is named, so when the typing issue bites someone, they know it was a known cost, not an oversight.

### Iterative refinement over big-bang design

Good design emerges through rounds of feedback, not from a single brilliant session.

- **Round 1**: Rough shape. Expect it to be challenged.
- **Round 2**: Incorporate feedback, address gaps. Still expect pushback.
- **Round 3**: Refined design with trade-offs resolved. Naming audited.
- **Round 4+**: Edge cases, integration testing, documentation.

Each round has a narrower focus than the previous one. If round 3 revisits round 1 questions, the process failed -- the earlier rounds didn't capture decisions properly.

### Knowledge flows in teams

- **Before a team session**: Load prior decisions, research prior art, collect full context. The team cannot evaluate in a vacuum.
- **During**: Capture dissent explicitly. Smoothed-over disagreement resurfaces later as bugs.
- **After**: Meeting notes with next steps, open questions in a separate tasks file, and links to this round from the next one. Every round builds on the previous -- no team starts from zero.

### Curate, don't hoard

- Generic knowledge goes to reusable storage (Resources), project-specific stays with the project
- Relative dates become absolute dates when written down ("Thursday" becomes "2026-04-25")
- Stale knowledge is worse than no knowledge -- update or remove
- If the same question comes up twice, the documentation failed the first time
