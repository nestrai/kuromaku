---
format: kuromaku-flow/v1
---

# implement-issue

Implement issue #{{vars.id}} in this repository. Follow the team's rules
and produce a draft PR at the end.

---

## design
*role: architect*

Read issue #{{vars.id}} (`gh issue view {{vars.id}} --comments`). Produce a
short design plan: affected files, interfaces, tests. Do not modify
code.

-> implement: plan complete, ready to implement
-> aborted: missing context, cannot plan

---

## implement
*role: developer*

Implement the design from the previous step. Conventional commits,
feature branch, never main. Add or update tests.

-> review: all design items implemented and committed
-> design: implementation revealed a design flaw, need to revisit
-> aborted: cannot proceed safely

---

## review
*role: reviewer*

Review the implementation against the issue's acceptance criteria.

-> verify: all acceptance criteria met, code quality acceptable
-> implement: code-level changes needed (naming, refactor, missing test)
-> design: implementation revealed the design itself is wrong
-> aborted: review cannot complete

---

## verify
*run: just lint && just test*

-> create-pr: pass
-> implement: fail

---

## create-pr
*role: developer*

Push the branch and open a draft PR with `Closes #{{vars.id}}`. Output
the URL.

-> done: PR opened
-> aborted: PR creation failed

---

## done
*final: Happy-path exit -- implementation reviewed and a draft PR is open.*

---

## aborted
*final: Early exit because a step could not proceed safely (missing context, design flaw it cannot resolve, PR push failed).*
