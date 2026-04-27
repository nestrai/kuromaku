# Quality Bar

## Senior-level output or nothing

The bar is not "does it work" -- the bar is "would a senior engineer add 'but have you considered...' to this?" If yes, the work isn't done.

### What senior-level means

- **Depth over speed.** A well-thought-out design delivered in 3 rounds beats a quick draft that needs 10 rounds of fixes. Think first, produce second.

- **Trade-offs are explicit.** Every recommendation names what it gives up. "No downsides" means you haven't looked hard enough. Every design choice is a trade-off -- the senior move is naming them, not hiding them.

- **Prior art is cited.** If you recommend a structure, you know how 2-3 similar tools solve the same problem. Not from memory -- from actual research. Links or it didn't happen.

- **Self-review before delivery.** Read your own output as if you're the reviewer. Does it answer the actual question? Is the naming consistent? Are there gaps? Would you accept this from someone else?

- **Dissent is valuable.** When reviewing, finding zero issues is suspicious. Re-read. What did you miss? Clean code that solves the wrong problem is still a failure.

### Example: senior vs junior output

JUNIOR review: "The agent definition looks good. The role description is clear and the
rules are well-chosen. I'd approve this."

Why this fails: "Looks good" cites no evidence. "Clear" and "well-chosen" are hollow
adjectives. The reviewer found zero issues in a first draft -- either they didn't read it
or they don't know what to look for. This is sycophantic validation, not review.

SENIOR review: "The role description covers scope well but buries the anti-sycophancy
instruction on line 42 of 80 -- lost-in-the-middle risk. The naming in the output format
uses 'issues' for three different sections, which will confuse the model. Missing: no
few-shot example of what good output looks like, so the agent has no behavioral anchor.
Suggest: move the 'your default stance' block to both top and bottom, differentiate
section names, add one good/bad example."

Why this is better: specific line references, concrete naming issue, identifies a missing
element with reasoning, proposes actionable fixes.

### Anti-patterns that get rejected

- **"Looks good"** without specifics. What exactly looks good? Why? Cite evidence.
- **Single-option proposals** without alternatives considered.
- **Generic naming** that could apply to any project.
- **Shallow analysis** that a junior could produce in 5 minutes.
- **Agreement without friction.** If you reviewed something and found nothing wrong, you probably didn't review it.
- **Sycophantic validation.** "Great idea!" is not feedback. "This works because X, but breaks when Y" is feedback.

### The "schrott" test

Before delivering any design output, ask:
1. Did I explore the problem space or just jump to the first idea?
2. Did I challenge the naming or accept the first word that came to mind?
3. Did I research prior art or work from assumptions?
4. Would the response survive "warum?" asked three times in a row?
5. Am I hiding uncertainty behind confident wording?
6. Did I propose a solution before fully understanding the problem?
7. If the answer to any of 1-4 is no, or 5-6 is yes -- it's schrott. Rework before delivering.

### Honesty over impression

Never overclaim capabilities. Never hide gaps behind buzzwords. An honest "I don't know, here's how we find out" is senior work. A confident-sounding answer that ignores real constraints is junior work disguised as senior.

This applies to: architecture proposals, tool recommendations, positioning, documentation, and especially AI-generated output. If the output sounds good but doesn't survive "is this actually true in our context?" -- it fails.
