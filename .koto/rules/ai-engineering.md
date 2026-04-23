# AI Engineering Rules
<!-- Derived from Chip Huyen "AI Engineering" (O'Reilly 2025), Anthropic "Building Effective Agents", and Multi-Agent Team Prompts knowledge base -->

## Prompt Engineering

### Clarity and Structure

- **Write explicit instructions.** "Score this essay on a scale from 1 to 5 (integers only)" beats "score this essay". Add constraints for any undesirable behaviors observed during testing.
- **Specify output format.** Define JSON keys, response length, structure. For pipeline inputs, add markers (`-->`) to signal where the model response begins.
- **Provide context.** Include the document if answering questions about it. Ground responses in provided material to reduce hallucinations.
- **Use personas deliberately.** "You are a senior Rust developer" shifts the model's frame of reference. Personas change outputs, they are not decoration.
- **Examples beat descriptions.** Show the desired behavior, don't just describe it. Format matters: `item --> label` uses ~30% fewer tokens than `Input: item\nOutput: label` with equivalent performance.

### Context and Position

- **Critical instructions go at the top or bottom.** Models handle beginning and end better than middle positions (lost in the middle problem).
- **Keep prompts under ~800 tokens per concern.** Longer prompts deserve decomposition into subtasks.
- **Break complex tasks into chains.** Decompose big prompts into sequential smaller ones. Benefits: monitoring, debugging, parallelization, cost optimization (cheaper models for simple steps).

### Chain-of-Thought

- **Give the model time to think.** "Think step by step" consistently improves reasoning and reduces hallucinations.
- **Structured CoT for complex tasks.** Specify the reasoning steps explicitly or include a worked example (one-shot CoT).
- **Self-critique for high-stakes tasks.** Ask the model to review its own output before finalizing. Adds latency but improves reliability.

### Version Control

- **Treat prompts like code.** Store them separately from application logic. Version them. Use experiment tracking with standardized metrics.
- **Test in isolation.** An improvement in one subtask doesn't guarantee system-wide gains.
- **Prompt changes break with model updates.** Revalidate when switching models or when a model is updated.

### Anti-Patterns

- **Vague instructions.** "Summarize this" is worse than "Summarize in 2-3 sentences, focusing on the main argument."
- **Prompt bloat.** Prompts that grow unboundedly as every edge case triggers another instruction. Redesign instead of patching.
- **Not checking the rendered prompt.** Template bugs, missing variables, unexpected whitespace cause silent failures. Always log and inspect the final prompt.
- **Happy-path-only evaluation.** Most failures happen on edge cases: empty inputs, malformed data, unexpected languages. Build evaluation sets that cover failure modes.
- **Magical thinking about negative instructions.** "Do not make up information" helps but doesn't guarantee compliance. Pair with positive alternatives and output validation.

## Agent Design

### When to Use Agents

- **Agents trade latency and cost for capability.** Many tasks benefit more from optimized single LLM calls with retrieval and in-context examples.
- **Use agents for unpredictable step counts.** When you cannot hardcode the workflow path. Otherwise, use prompt chaining.
- **Start simple.** Optimize single LLM calls before adopting multi-step systems. Only increase complexity when it demonstrably improves outcomes.

### Core Principles

- **Single responsibility.** Each agent has one clear expertise area. Define role, title, and expected behavior explicitly.
- **Minimal tool sets.** More tools = more capability but also more confusion. Remove tools that don't contribute (use ablation studies).
- **Decouple planning from execution.** Validate plans before running them. Never couple them in the same prompt.
- **Compound errors dominate.** With 95% per-step accuracy, a 10-step workflow is only 60% reliable. Build in reflection and error correction from the start.

### Tool Design

- **Tools warrant equal prompt engineering effort as overall prompts.** Include example usage, edge cases, input requirements, boundaries.
- **Format matters.** Choose formats close to natural text patterns. Avoid overhead like manual line counting or excessive escaping.
- **Poka-yoke design.** Make mistakes harder (absolute filepaths instead of relative, explicit parameter schemas).
- **Test thoroughly.** Run multiple examples to identify failure patterns. Log every tool call and output.

### Workflow Patterns

- **Prompt Chaining**: Sequential steps where each LLM processes previous output. Fixed subtasks, trade latency for accuracy.
- **Routing**: Classify inputs, direct to specialized handlers. Customer support categories, model selection.
- **Parallelization**: Independent subtasks run simultaneously. Dramatically reduces latency for independent work.
- **Orchestrator-Workers**: Central LLM dynamically breaks tasks into subtasks and delegates. For unpredictable workflows.
- **ReAct (Reasoning-Acting)**: Interleave thought, action, observation. Visible reasoning makes debugging tractable. Default starting point.

### Reflection and Error Correction

- **Reflection is cheaper than better planning.** Evaluate progress at multiple points: after query, after plan, after each step, after completion.
- **Reflexion pattern.** Evaluator scores outcome, self-reflection analyzes why it failed, agent proposes new plan.
- **Detect reflection failures.** Agent incorrectly believes it succeeded. Particularly dangerous because failure is invisible.

### Memory Management

- **Don't rely on FIFO.** If early context contains critical task definitions, use summary-based or reflection-based strategies.
- **Context overflow is hard to fix retroactively.** Build memory management before you need it.
- **Persist across sessions.** Stateless agents that forget everything are frustrating to use.

## Flow Design

### Decomposition

- **One flow per concern.** Separate workflows for different tasks. Don't monolith.
- **Minimize steps.** Each step introduces error. (0.95)^10 = 60% accuracy. Fewer steps = higher reliability.
- **Parallel where possible.** Independent steps run concurrently. Cut latency dramatically.
- **Intent classification first.** Route queries to right tools and strategy. Reject out-of-scope requests early.

### Validation

- **Plan-then-execute pattern.** Generate plan, validate, execute, evaluate results. Never execute unvalidated plans.
- **Validation options:** Heuristics (reject invalid tool calls), AI judges (second model evaluates), human review (high-stakes).
- **Human-in-the-loop for irreversible actions.** Database updates, code merges, financial transactions require approval.

### Compound Error Awareness

- **Errors multiply, they don't add.** With 95% per-step accuracy: 10 steps = 60%, 100 steps = 0.6% accuracy.
- **Use strongest models for agent planners.** Weak models compound errors badly over many steps.
- **Track failure attribution.** When end-to-end tests fail, log which agent's output first diverged from expected.

## Rule Composition

### Structure

- **Concise and actionable.** No filler, no generic advice. Specific to the domain.
- **Examples over descriptions.** Show the pattern, don't just describe it.
- **Bullet points over paragraphs.** Easier to parse, clearer to follow.
- **Comment blocks for source attribution.** Help maintainers understand where rules came from.

### Content

- **Grounded in sources.** Reference books, articles, internal knowledge base. Not generic "best practices".
- **Domain-specific.** Rust rules for Rust agents, AI engineering rules for AI agents. No one-size-fits-all.
- **Anti-patterns explicit.** Don't just say what to do, say what NOT to do and why.

### Maintenance

- **Keep rules in sync with reality.** Outdated rules are worse than no rules.
- **Small, focused rule files.** One concern per file. Combine via agent config, don't monolith.
- **Cross-reference where needed.** git-workflow rule applies to many agents, rust-developer rule applies to Rust agents.

## Anti-Patterns in AI Systems

### Sycophancy (RLHF-Induced Agreement)

- **LLMs default to agreeing.** RLHF trains models to be helpful and agreeable. Without countermeasures, agents validate user ideas instead of challenging them.
- **Binary framing forces evaluation mode.** "Option A vs B?" makes the agent pick between given options instead of proposing new ones.
- **Unanimous agreement is a red flag.** If all agents agree, increase challenge pressure or add Devil's Advocate.
- **Countermeasures:**
  - Never frame questions as binary. Always: "Evaluate these options AND propose alternatives."
  - Explicitly instruct: "Challenge the question itself — is this the right question?"
  - "The user's suggestions get the same scrutiny as any other option."
  - Include few-shot examples of good critical analysis and bad sycophantic output.

### Scope Creep

- **Ambiguous tasks interpreted broadly.** "Delete old records" might delete more than intended without explicit constraints.
- **Countermeasures:** Define success criteria explicitly. Validate plans before execution. Human approval for destructive actions.

### Incomplete Context

- **Agents optimize for what they see.** If shown only one aspect of a product, they optimize that aspect and miss the bigger picture.
- **Countermeasures:** Give agents the full scope: all features, vision, roadmap, GitHub issues, ADRs. Partial context produces partial solutions.

### Vague Instructions

- **"Write good code" is not actionable.** Specify what "good" means: tested, follows style guide, handles edge cases, includes docs.
- **Countermeasures:** Use checklists, acceptance criteria, examples of good vs bad output.

### Over-Engineering

- **Complexity is a liability.** Every abstraction layer, every agent, every tool is a point of failure.
- **Countermeasures:** Start simple. Add complexity only when it demonstrably improves outcomes. Measure before and after.

## Evaluation

### Metrics

- **Track per-component and end-to-end.** Unit tests per agent, integration tests for handoffs, end-to-end for full workflows.
- **Planning metrics:** % valid plans, average attempts before valid plan, tool call accuracy (valid tool, valid parameters, correct values).
- **Efficiency metrics:** Average steps per task, cost per task, time per action type, tool contribution to cost/latency.
- **Security metrics:** Violation rate (successful attacks / total attempts), false refusal rate (legitimate requests refused). Balance both.

### Testing

- **Edge cases, not just happy paths.** Empty inputs, malformed data, adversarial phrasing, unexpected languages.
- **Ablation studies.** Remove each component and measure performance drop. Remove what doesn't contribute.
- **Error analysis.** Look for consistently misused tools or agents. Redesign or split them.
- **Call distribution analysis.** Unused tools/agents are wasted context. Remove them.

### Iteration

- **Measure before adding complexity.** Baseline performance first. Add feature. Measure again. Keep only if improvement is demonstrable.
- **Failure attribution logs.** When end-to-end fails, trace which component diverged first. Rich logging at every handoff is non-negotiable.
