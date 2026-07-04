# Java + Go corpus additions (intake-corpus-v2)

Adds Java and Go coverage to the v2 code-profiling corpus, which previously had
**zero** cases for either language even though `code::required_toolchain` now has
`java → javac` and `go → go` gates (added in the same change). This directory
mirrors the deployed corpus layout (`_workspaces/`, `<lang>/<tier>/<case>/`) so
the new content is reviewable in git.

## Why this lives in the repo (and where the live corpus lives)

The operational corpus lives ONLY as deployed data on <host> at
`<path>/intake-corpus-v2/` — it is **not** git-tracked in this repo, and the
live `manifest.json` there is the single source of truth for a running sweep.
This task authors content for review/merge, not deployment, so:

- The new **case files** and **workspaces** are committed here under
  `intake-corpus-v2/` mirroring the live layout exactly, so a reviewer can read
  them and the operator can copy them straight across.
- The 10 new manifest rows are in **`manifest-additions.json`** (a fragment, not
  a full manifest) rather than a copy of the live 40-row `manifest.json`, so
  there is no risk of a stale full-manifest copy clobbering the live index. The
  operator splices these rows into the live `manifest.json` array.

## Rolling these into the live corpus (operator, after review)

```
CORPUS=<path>/intake-corpus-v2
cp -r intake-corpus-v2/_workspaces/wx-java  $CORPUS/_workspaces/
cp -r intake-corpus-v2/_workspaces/wx-go    $CORPUS/_workspaces/
cp -r intake-corpus-v2/java $CORPUS/
cp -r intake-corpus-v2/go   $CORPUS/
# splice the 10 rows from manifest-additions.json into $CORPUS/manifest.json
# (jt: jq -s '.[0] + .[1]' $CORPUS/manifest.json manifest-additions.json)
chmod +x $CORPUS/java/*/*/validate.sh $CORPUS/go/*/*/validate.sh
$CORPUS/prewarm.sh   # smoke-build the new workspaces (needs javac + go on PATH)
```

Requires `javac`/`java` and `go` on PATH on the benchmark host — NOT currently
installed on <host> (apt is broken there; toolchain install is out of scope for
this task).

## Cases (10: 5 Java, 5 Go — mixed blitz/standard)

| id               | tier     | target             | task |
|------------------|----------|--------------------|------|
| java-blitz-j1    | blitz    | `Validate.java`    | add `validateEmail` (normalize + reject) |
| java-blitz-j2    | blitz    | `Validate.java`    | add `clampPercent` (parse + range-check) |
| java-blitz-j3    | blitz    | `Slug.java` (new)  | `slugify` string→URL slug |
| java-standard-j1 | standard | `RingBuffer.java` (new) | fixed-capacity int ring buffer class |
| java-standard-j2 | standard | `WordCount.java` (new)  | `count` + `topN` with tie-break sorting |
| go-blitz-g1      | blitz    | `validate.go`      | add `ValidateEmail` |
| go-blitz-g2      | blitz    | `validate.go`      | add `ClampPercent` |
| go-blitz-g3      | blitz    | `slug.go` (new)    | `Slugify` |
| go-standard-g1   | standard | `ring.go` (new)    | `RingBuffer` struct + methods |
| go-standard-g2   | standard | `wordcount.go` (new) | `Count` + `TopN` with tie-break sorting |

## Validators (stage contract)

Each `validate.sh` follows the README stage contract:
`STAGE:COMPILE` / `STAGE:TESTS` / `STAGE:CHANGE`, `TOOLCHAIN:missing <bin>` →
exit 3. The `STAGE:CHANGE` check is an INDEPENDENT hidden test the model never
sees (a `MintCheck.java` compiled+run for Java, a `mint_change_test.go`
`go test -run TestMintChange` for Go). `STAGE:TESTS` re-runs the baseline
workspace behavior (Java: a `MintBase.java` smoke; Go: the workspace's
`validate_test.go`) as a regression guard.

## Verification status

**Authored by inspection only — NOT executed.** No `javac`/`java`/`go`
toolchain was available on the dev box or <host> when these were written, so the
Java/Go sources embedded in the validators and the case specs were verified by
careful reading, NOT by compiling or running them. Treat every case as
UNTESTED-BY-EXECUTION until a toolchain is installed and `prewarm.sh` +
one correct-solution pass per case confirm each validator passes on a good
solution and fails on the baseline (the README "Adding a case" step 5).

The Rust change to `code::required_toolchain` (the `java`/`go` arms) IS covered
by unit tests and `cargo test --workspace`.
