# Design: Add --dry-run flag to `up` command

## Requirement
Users should be able to preview what `koto up` will do without executing.

## Design
- Add `dry_run: bool` field to `UpCommand` struct in `src/main.rs`
- Parse `--dry-run` flag via clap derive API using `#[arg(long)]` attribute
- In `run_up()` function (src/main.rs), check `dry_run` before executing each step
- If dry_run is true:
  - Print "DRY RUN: would execute <step name>" to stdout
  - Skip actual step execution (do not call executor)
  - Proceed to next step in the DAG
- If dry_run is false: normal execution path

## Error handling
- dry_run does not affect error parsing or DAG validation
- DAG validation runs in both modes (catch invalid flow definitions)
- Executor is only skipped when dry_run=true, all other pre-flight checks run normally

## Edge cases
- Empty flow: print "DRY RUN: no steps to execute"
- Flow with failed dependencies: still show dependency errors (DAG validation runs)
- Combined with other flags (--verbose, --stack-path): dry_run only affects execution

## Tests
- Unit test: `run_up` with dry_run=true does not call executor::execute
- Integration test: `koto up --dry-run <flow>` outputs expected "would execute" messages
- Integration test: verify no stack state modified after dry-run
- Error case: invalid flow file with --dry-run should still fail at parse stage

## Acceptance criteria
- `koto up --dry-run` prints all steps that would execute
- `koto up --dry-run` does not modify stack state
- `koto up --dry-run` exits with code 0 if flow is valid
- `koto up --dry-run` exits with non-zero if flow is invalid (validation still runs)
