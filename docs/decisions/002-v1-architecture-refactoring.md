# ADR-002: V1 Architecture Refactoring Plan

**Status:** accepted
**Date:** 2026-04-22
**Team:** AI Architect, Platform Engineer, Devil's Advocate, User Advocate

## Context

Issue #35 requested an architecture review before implementing v1 features. The team evaluated the current 2,850 LOC codebase against 7 planned features:

1. Parallel execution (#28)
2. Content-addressable caching (#22)
3. Run-ID based stack (#31)
4. tmux live output (#29)
5. Human-in-the-loop (#20)
6. Token reporting (#19)
7. Registry / shareable resources (#11)

### Current Architecture

Single Rust crate with modules:
- `main.rs`: CLI, flow resolution, orchestration (250 LOC)
- `config.rs`: YAML parsing, validation (680 LOC)
- `dag.rs`: Topological sort, cycle detection (320 LOC)
- `runner.rs`: Sequential step execution loop (640 LOC)
- `executor/`: Process spawning, command building (230 LOC)
- `stack.rs`: Persistent output storage (110 LOC)
- `skills.rs`: Git-based skill fetching (320 LOC)
- `llm.rs`: API client (~100 LOC)
- `ui.rs`: Terminal output (~200 LOC)

**Key finding:** The architecture is fundamentally sound for current usage, but `runner.rs` is a monolith that blocks parallelism, caching, and human-in-the-loop features.

## Decision

### Phase 1: Essential Refactorings (v1 blocking)

These refactorings enable the most valuable features with minimal complexity.

#### 1.1 Extract RunContext struct

**Problem:** `runner::run_steps()` takes 8 parameters. Adding features (run-ID, caching, approval) would add more.

**Solution:**

```rust
pub struct RunContext {
    pub run_id: String,           // YYYYMMDD-HHMMSS-<3-char-hash>
    pub flow_name: String,
    pub task: String,
    pub stack: Arc<Stack>,        // owns run-ID pathing
    pub cache: Option<Arc<Cache>>, // None if caching disabled
    pub guide: Option<String>,
    pub rules_cache: HashMap<String, String>,
    pub skills_cache: HashMap<String, String>,
}
```

**Impact:** Unblocks all other refactorings. Single source of truth for run-level state.

#### 1.2 Run-ID based stack (#31)

**Current:** Flat `~/.koto/stacks/<project>/<step>.json` - runs overwrite each other.

**V1 structure:**

```
~/.koto/stacks/<project>/
  runs/
    20260422-143052-a3f/
      meta.json       # flow, task, start/end time, status
      design.json     # step metadata
      design.md       # step artifact
      implement.json
      implement.md
  latest -> runs/20260422-143052-a3f/  # symlink
```

**Implementation:**
- `Stack` struct owns `run_id`
- `write_step()` writes to `runs/<run_id>/<step>.json`
- `latest/` symlink updated on successful completion
- `koto runs` lists run directories, `koto show <run-id>` reads from path

**Rationale:** Enables run history, comparison, rollback. Minimal complexity, high value. Consensus across all team members.

#### 1.3 Token reporting (#19)

**Current:** CLI backends return `None` for usage. Summary shows "—".

**Solution:**
- Change `build_claude_command()` to use `--output-format json` instead of `text`
- Parse JSON response, extract `usage` field
- Return `Usage { input_tokens, output_tokens }` from executor
- For Ollama (no token reporting): return estimate based on output length

**Display:**

```
Run complete: review-pr pr=67
Duration: 2m 34s
Tokens: 125,000 input + 8,200 output = 133,200 total
Estimated cost: $1.25 (claude-sonnet-4-5 @ ~$3/$15 per 1M)
```

**Rationale:** User Advocate identified this as v1 blocking ("Silent cost is adoption poison"). AI Architect noted zero cost visibility prevents budget enforcement. Devil's Advocate agreed this is one of only two essential features.

#### 1.4 Human-in-the-loop approval gates (#20)

**Config:**

```yaml
flow:
  consensus:
    agent: Mika
    approval: after  # pause after step completes
```

**Implementation:**

```rust
// approval.rs
pub enum ApprovalDecision {
    Continue,
    EditAndRerun(String),
    Abort,
}

pub async fn wait_for_approval(step: &Step, output: &StepOutput) -> Result<ApprovalDecision> {
    println!("\n{}", output.response); // show first 50 lines

    let choice = prompt_select("Approve to continue?", vec![
        "Continue",
        "Edit output",
        "Abort flow",
    ])?;

    match choice {
        "Continue" => Ok(ApprovalDecision::Continue),
        "Edit output" => {
            let edited = prompt_editor(&output.response)?;
            Ok(ApprovalDecision::EditAndRerun(edited))
        }
        "Abort" => Ok(ApprovalDecision::Abort),
    }
}
```

**Integration:** Check `step.approval` in `run_steps()`, call `wait_for_approval()` after LLM execution, before writing to stack.

**Rationale:** User Advocate: "Without this, flows with side effects are dangerous." This is the only thing that prevents koto from being untrustworthy. Devil's Advocate challenged it as contradicting "reproducible," but consensus: reproducibility applies to approved runs. Approval is user input, just like `pr=67` template args.

### Phase 2: Performance Optimizations (v1 optional)

These improve performance but are not blocking.

#### 2.1 Parallel execution (#28)

**Current:** `for (i, step) in steps.iter().enumerate()` -- strictly sequential.

**Solution:** Spawn independent steps concurrently using `tokio::JoinSet`.

```rust
// coordinator.rs
pub async fn execute_dag(
    steps: &[Step],
    ctx: &RunContext,
) -> Result<Vec<StepResult>> {
    let mut join_set = JoinSet::new();
    let mut completed = HashSet::new();
    let concurrency_limit = Arc::new(Semaphore::new(4)); // configurable

    loop {
        // Find ready steps (all dependencies satisfied)
        let ready: Vec<&Step> = steps.iter()
            .filter(|s| !completed.contains(s.id.as_str()))
            .filter(|s| s.needs.iter().all(|dep| completed.contains(dep.as_str())))
            .collect();

        if ready.is_empty() && join_set.is_empty() {
            break;
        }

        // Spawn ready steps
        for step in ready {
            let permit = concurrency_limit.clone().acquire_owned().await?;
            let ctx = ctx.clone();
            join_set.spawn(async move {
                let result = run_single_step(step, &ctx).await;
                drop(permit);
                result
            });
        }

        // Wait for at least one completion
        if let Some(result) = join_set.join_next().await {
            let step_id = result??;
            completed.insert(step_id);
        }
    }

    Ok(results)
}
```

**Configuration:**

```yaml
defaults:
  concurrency: 4  # max parallel steps (default 4)
```

**Automatic behavior:** If not configured, automatically run independent steps in parallel (as Devil's Advocate noted: just use the DAG info we already have).

**Rationale:** AI Architect: "Parallel execution is underutilized." Platform Engineer: detailed implementation plan. Devil's Advocate: "Only 1 diamond in 3 flows" — true, but zero-config auto-parallelization has no downside. Make it automatic.

**Priority:** Optional for v1.0, target v1.1.

#### 2.2 Content-addressable caching (#22)

**Cache key:**

```rust
fn compute_cache_key(step: &Step, agent: &Agent, ctx: &RunContext) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(agent.role.as_bytes());
    hasher.update(agent.model.as_bytes());
    hasher.update(step.task.as_deref().unwrap_or("").as_bytes());

    // Hash prior step outputs (content-addressable)
    for input_id in &step.input {
        if let Ok(output) = ctx.stack.read(input_id) {
            hasher.update(output.response.as_bytes());
        }
    }

    // Hash Guide.md, rules, skills
    if let Some(guide) = &ctx.guide {
        hasher.update(guide.as_bytes());
    }
    for (name, content) in &ctx.rules_cache {
        hasher.update(name.as_bytes());
        hasher.update(content.as_bytes());
    }

    CacheKey(hasher.finalize().to_hex().to_string())
}
```

**Cache lookup:**

```rust
// Before LLM call in run_single_step()
let cache_key = compute_cache_key(step, agent, ctx);
if let Some(cache) = &ctx.cache {
    if let Some(cached_output) = cache.get(&cache_key).await? {
        ui::print_cache_hit(&step.id);
        return Ok(StepResult::from_cache(step.id, cached_output));
    }
}
```

**Cache storage:** `~/.koto/cache/<hash>.json` (StepOutput)

**Transparency:**

```bash
koto up review-pr pr=67 --verbose
# Output:
#   Step 'fetch': cache miss (input changed)
#   Step 'design': cache hit (key: a3f92b...)
```

**Rationale:** Devil's Advocate: "Cache hit rate might be <10% due to LLM non-determinism." User Advocate: "Cache model is broken if users can't understand invalidation." **Consensus:** Implement with conservative invalidation (hash everything), add verbose mode to explain hits/misses. If hit rate is <20% after 2 weeks of dogfooding, remove the feature.

**Priority:** Optional for v1.0. Requires measurement. Target v1.1 with kill criterion.

### Phase 3: Advanced Features (post-v1)

#### 3.1 tmux live output (#29)

**Rationale:** User Advocate: "Works for power users, breaks for IDE users." Make opt-in via `--live-output` flag. Default behavior: spinner + final output (works everywhere).

**Implementation:** New `TmuxExecutor` implementing existing `Executor` trait (no breaking changes).

**Priority:** v1.1 or later.

#### 3.2 Registry / shareable resources (#11)

**Rationale:** Devil's Advocate: "Git-based skills already exist. GitHub is the registry." User Advocate: "Just use GitHub, don't build infrastructure."

**V1 approach:** `koto pull` fetches from `.koto/skills.lock` (already implemented). For agents/flows, manual copy-paste from GitHub examples.

**Future:** Multi-source resolution (local → ~/.koto/ → GitHub), but defer until second user requests it.

**Priority:** v1.1 or later.

### Refactorings NOT Recommended

**Workspace crate split:** Devil's Advocate: "At 4000 LOC, this is premature." Consensus: defer until 10k LOC or external library consumers exist.

**Executor trait redesign:** Platform Engineer proposed `Backend` trait to unify API + CLI. Devil's Advocate: "Over-engineered for local-only." Consensus: current trait is fine, add `TmuxExecutor` as new impl without breaking changes.

**Conditional routing (#26):** Low priority, most flows are linear. Defer to post-v1.

## Implementation Plan

### Sprint 1: Essential Refactorings (2 weeks)

**Week 1:**
- [ ] Extract `RunContext` struct (1 day)
- [ ] Implement run-ID based stack with `meta.json` (2 days)
- [ ] Add `koto runs` and `koto show <run-id>` commands (1 day)
- [ ] Token reporting: parse claude-cli JSON output (1 day)

**Week 2:**
- [ ] Implement `approval.rs` module (2 days)
- [ ] Add `approval: after` config field to Step (1 day)
- [ ] Integration testing with approval flows (1 day)
- [ ] Update examples with token counts + approval gates (1 day)

### Sprint 2: Performance Optimizations (2 weeks, optional)

**Week 3:**
- [ ] Parallel execution: Coordinator with JoinSet (3 days)
- [ ] Concurrency limits with Semaphore (1 day)
- [ ] Testing with parallel flows (1 day)

**Week 4:**
- [ ] Content-addressable caching: Cache module (2 days)
- [ ] Cache key computation + lookup (1 day)
- [ ] Cache hit/miss visibility in verbose mode (1 day)
- [ ] **Measurement phase begins:** track cache hit rate

### Sprint 3: Evaluation & Polish (1 week)

- [ ] Dogfooding: run koto on koto for 1 week
- [ ] Measure cache hit rate across 20+ runs
- [ ] If cache hit rate <20%: remove caching (#22)
- [ ] If parallel execution saves <30s across dogfooding flows: make opt-in instead of default
- [ ] Documentation updates
- [ ] Tag v1.0

## Success Criteria

**v1.0 ships with:**
- Run-ID based stack (all runs isolated, browsable via `koto runs`)
- Token reporting (every run shows cost estimate)
- Human-in-the-loop (approval gates prevent dangerous actions)
- Parallel execution (automatic, zero-config) OR justification for deferral
- Content-addressable caching OR justification for removal

**Kill criteria:**
- Caching: remove if hit rate <20% after 2 weeks dogfooding
- Parallel execution: make opt-in if wall-clock savings <30s per flow

## Consequences

**Positive:**
- Run isolation prevents data loss (Issue #31 solved)
- Token reporting builds trust (cost visibility)
- Approval gates enable flows with side effects (GitHub PR comments, database writes)
- Parallel execution speeds up diamond-shaped flows (when applicable)
- Conservative refactoring preserves working codebase

**Negative:**
- RunContext struct adds indirection (but reduces parameter count 8→2)
- Run-ID directories add nesting (`~/.koto/stacks/<project>/runs/<id>/` vs flat)
- Approval gates break non-interactive use (need `--yes` flag for CI)
- Caching may be removed if hit rate is low (wasted implementation effort)

**Trade-offs:**
- Prioritized user trust (approval, token reporting) over performance (parallel, caching)
- Deferred ecosystem features (registry, tmux) to focus on core workflow
- Conservative scope (defer workspace split, executor redesign) to ship sooner

## Dissent

**Devil's Advocate position:** Cut scope to just token reporting + run-ID stack. Implement nothing else until second user appears. The current architecture is not broken.

**Team consensus:** Token reporting + run-ID + approval gates are essential. Parallel execution and caching are optional, with kill criteria. Ship v1.0 with essentials, v1.1 with optionals if justified by data.

## Open Questions for User

1. **Approval UX:** Should `approval: after` block the entire flow, or allow other parallel steps to continue? (Recommendation: block, prevents wasted tokens if user aborts)

2. **Cache storage:** Local `~/.koto/cache/` only, or support remote cache (S3, Garage) for team sharing? (Recommendation: local for v1, remote post-v1)

3. **Concurrency default:** 4 parallel steps reasonable default, or should it be `min(4, available_cores)`? (Recommendation: fixed 4, configurable via `defaults.concurrency`)

4. **Run retention:** Should old runs auto-delete after N days, or manual cleanup only? (Recommendation: manual via `koto runs clean --older-than 30d`)

5. **Non-interactive mode:** `koto up --yes` to skip all approval prompts (for CI), or separate config `approval_required: false`? (Recommendation: `--yes` flag, override all approvals)

## References

- Issue #35: Architecture review before v1 features
- AI Engineering book (Chip Huyen): compound errors, parallel execution, human-in-the-loop patterns
- Multi-Agent Team Prompts: team assembly, anti-sycophancy rules
- Existing ADR-001: Concrete models in agent files
