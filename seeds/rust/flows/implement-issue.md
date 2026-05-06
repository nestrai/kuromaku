---
format: kuromaku-flow/v1
---

# implement-issue

Implement issue #{{vars.id}} in this repository. Follow the team's
rules and produce a draft PR at the end.

---

## design
*role: architect*

Read issue #{{vars.id}} with `gh issue view {{vars.id}} --comments`.

Produce a short design plan: affected files, interfaces, tests.

Do not modify code.

-> implement: plan complete, ready to build
-> aborted: missing context, cannot plan

---

## implement
*role: developer*

Implement the design from the previous step. Conventional commits,
feature branch, never main. Add or update tests.

-> review: implementation complete
-> design: design flaw discovered
-> aborted: cannot proceed safely

---

## review
*role: reviewer*

Review the implementation against the issue's acceptance criteria.

-> verify: all acceptance criteria met
-> implement: code-level changes needed
-> design: wrong abstraction
-> aborted: review cannot complete

---

## verify
*run: just lint && just test*

-> create-pr: pass
-> implement: fail

---

## create-pr
*role: developer*

Push the branch and open a draft PR with `Closes #{{vars.id}}`.

-> done: PR opened
-> aborted: PR creation failed

---

## done
*final: implementation reviewed, draft PR is open*

---

## aborted
*final: a step could not proceed safely*
