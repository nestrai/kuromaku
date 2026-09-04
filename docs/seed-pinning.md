# Seed pinning

How to consume a shared seed library reproducibly until remote seed
resolution ships.

kuromaku's config schema already parses remote seed entries (`repo:` /
`ref:`), but resolving them is a deliberate stub: any flow that falls
through to a remote seed fails with `remote seeds not yet implemented`.
[ADR-0009](decisions/0009-version-pinning.md) explains why the remote
resolver is deferred and why git is the pinning mechanism in the
meantime. Everything below uses only `seeds[].path` entries, which are
fully supported today. When the remote resolver lands, `repo:` + `ref:`
will become the native way to pin; nothing in this document changes the
config schema, so migrating later is a config edit, not a rewrite.

## Recommended pattern: a commit-pinned Git submodule

Add the seed library as a Git submodule inside the consuming
repository. The parent repository's gitlink records the exact commit,
so every clone resolves identical seed content, and every seed update
is an ordinary reviewable diff.

Recommended layout:

```text
project/
  .kuro/
    config.yaml
  vendor/
    kuromaku-seeds/   # Git submodule pinned by the parent repository
```

The cascade in `.kuro/config.yaml` references the submodule with paths
relative to the project root:

```yaml
version: "1"
seeds:
  - path: .kuro/
  - path: vendor/kuromaku-seeds/coding/rust/
  - path: vendor/kuromaku-seeds/github/
  - path: vendor/kuromaku-seeds/coding/common/
```

Earlier entries win on name conflicts, so the project-local `.kuro/`
stays on top and the seed library fills in underneath.

### Setup

```sh
git submodule add <seed-repo-url> vendor/kuromaku-seeds
git commit -m "chore: add kuromaku-seeds submodule"
```

### Cloning a project that uses the pattern

```sh
git clone --recurse-submodules <project-url>
```

If the project was cloned without submodules, `kuro` fails fast at
config load with `seed path "vendor/kuromaku-seeds/coding/rust/" does
not exist`. That error is the signal to initialize the submodule:

```sh
git submodule update --init --recursive
```

### Updating the pin

Check out the desired seed-library commit inside the submodule, then
record the new gitlink in the parent repository:

```sh
git -C vendor/kuromaku-seeds fetch
git -C vendor/kuromaku-seeds checkout <commit>
git add vendor/kuromaku-seeds
git commit -m "chore: bump kuromaku-seeds to <commit>"
```

### Reviewing a pin change

The gitlink shows up as a one-line `Subproject commit` change in the
PR diff. To see what actually changed between the old and new pin:

```sh
git diff --submodule=log
```

Reviewers approve a seed bump the same way they approve any other
dependency bump: old commit, new commit, log in between.

## Alternative: a pinned local checkout

If a submodule does not fit (for example, the seed library must not be
vendored into the consuming repository), a sibling checkout works --
but only when it is pinned to an immutable commit.

```text
parent/
  kuromaku-seeds/     # separate clone, pinned manually
  project/
    .kuro/
      config.yaml
```

Reference it with a relative path (never a home-directory or otherwise
machine-specific absolute path -- the config is shared, the checkout
location is not):

```yaml
version: "1"
seeds:
  - path: .kuro/
  - path: ../kuromaku-seeds/coding/rust/
  - path: ../kuromaku-seeds/github/
  - path: ../kuromaku-seeds/coding/common/
```

Pin the checkout to a commit and record that commit in the consuming
repository (for example in the README or a `SEEDS_PIN` file), so the
pin is visible in review even though git does not track it for you:

```sh
git -C ../kuromaku-seeds checkout <commit-hash>
```

If you pin to a tag, resolve and record the commit it points to:

```sh
git -C ../kuromaku-seeds rev-parse '<tag>^{commit}'
```

The commit hash is the guarantee; the tag is only a label. Tags can be
moved or deleted, so a tag name alone does not pin anything -- record
the resolved commit.

**Warning: an unpinned branch checkout is not reproducible.** If the
sibling checkout tracks a branch, two clones of the consuming project
can resolve different seed content depending on when each machine last
pulled. The same flow then produces different agents, rules, and
prompts on different machines. Always pin.

Compared to the submodule pattern this trades vendoring for manual
discipline: git enforces nothing about the sibling checkout, and the
recorded pin can silently drift from what is actually checked out.
Prefer the submodule unless you have a concrete reason not to.

## Operational note: run `kuro` from the project root

Relative seed paths resolve against the directory `kuro` runs from,
which is also where `.kuro/config.yaml` is discovered. Run `kuro` from
the project root (the directory containing `.kuro/`), and both
patterns above resolve as written.
