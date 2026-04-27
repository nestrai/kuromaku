# Good Documentation Practices
<!-- Derived from Write the Docs community standards, Divio documentation system, and internal learnings -->

## The Four Types of Documentation

Every doc serves exactly one purpose. Mixing types produces docs that do nothing well.

- **Tutorial**: learning-oriented. Walks the reader through a complete exercise. "Follow these steps to deploy your first agent team."
- **How-to guide**: task-oriented. Solves a specific problem. "How to add a custom rule to your agent."
- **Reference**: information-oriented. Describes the system accurately and completely. "All fields in koto.yaml with types and defaults."
- **Explanation**: understanding-oriented. Clarifies concepts and decisions. "Why agents use rules instead of inline instructions."

Do not write a tutorial that also tries to be a reference. Do not write a how-to guide that explains the theory. If a doc tries to do two things, split it.

## Writing Rules

### Lead with the reader's question
- First sentence answers: what is this, and why should I care?
- Do NOT start with history, motivation, or "In order to understand X, we first need to..."

### Structure for scanning
- Headers create a navigable outline. A reader should understand the doc from headers alone.
- Use tables for structured comparisons, lists for sequences, code blocks for examples.
- Paragraphs longer than 5 lines need a subheading or should be a list.

### Examples over descriptions
- "The `rules` field takes a list of rule file names" is a description.
- `rules: [ai-engineering, git-workflow]` is an example. The example communicates faster.
- Show the most common case first. Show edge cases after.

### Cut filler
- Every word must earn its place. Delete: "In order to", "It should be noted that", "As mentioned above", "Please note", "basically", "essentially", "simply".
- If a sentence works without an adverb, remove the adverb.

### Write for the newcomer
- Define terms on first use. Do not assume the reader attended the design meeting.
- Link to prerequisites. "This guide assumes you have completed the [Getting Started tutorial](...)."
- If a section requires context from another doc, link it -- do not repeat it.

## Anti-Patterns

| Anti-pattern | Why it fails | Fix |
|---|---|---|
| Describing internals instead of usage | Users need to know HOW, not how it is implemented | Write from the user's perspective: what do they type, what do they see? |
| Wall of text with no headers | Unscannable. Reader gives up. | Add headers every 3-5 paragraphs. Each header is a question the reader has. |
| Outdated examples | Worse than no docs -- leads to errors and erodes trust | Automate example testing where possible. Date-stamp manual examples. |
| Assuming context | "As discussed" or "the usual approach" -- who discussed? Usual for whom? | Name the approach explicitly. Link to the decision record. |
| Screenshot-heavy guides | Screenshots break on every UI change | Prefer text/code examples. Use screenshots only for visual UIs. |
| Changelog as documentation | "Added X in v2.3" tells you nothing about how to use X | Write usage docs. Reference the changelog for history. |

## Maintenance

- Docs are part of the definition of done. A feature without docs is an undiscoverable feature.
- When code changes behavior, update the docs in the same PR. Not "in a follow-up."
- Review docs with the same rigor as code. Check: is it accurate? Is it complete? Can it be misunderstood?
- Delete docs for removed features. Outdated docs are actively harmful.
