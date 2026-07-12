# House style — Tier-A deterministic lint set (CXEG-05)

A small set of mechanical, deterministic rules enforced on every push via
`cargo test -p terminus-rs` (specifically `tests/house_style.rs`'s
`house_style_rules_hold()` test, which HARD-blocks the Stage-4 test gate on
any violation). Implemented as a `syn`-AST checker in `crate::house_style`
(`src/house_style/mod.rs`), not a `dylint`/rustc-driver custom lint — see
["Why not dylint"](#why-not-dylint) below.

## Running it locally

```sh
# Fast path — just this checker, no full test suite:
cargo run --bin house_style_check

# Or as part of the normal test gate:
cargo test -p terminus-rs house_style_rules_hold
```

Exit code `0` / assertion pass = clean. On a violation you get
`file:line: [rule-id] message` plus a `help:` line with the fix.

## The rules

### Rule 1 — no raw `std::env::var` for a secret-shaped name inside `execute`

A secret-shaped env var name (see [classification](#rule-1-classification)
below) must never be read with a raw `std::env::var("NAME")` call **inline
inside a `RustTool::execute`/`execute_structured` body**. It must instead be
read through a dedicated accessor defined elsewhere — this crate's existing,
established conventions:

- a `SomeConfig::from_env()` constructor (the dominant pattern — ~118 uses
  across the crate),
- a small single-purpose helper like `fn foo_token() -> Option<String>` /
  `fn foo_db_url() -> Result<String, ToolError>` (e.g. `wizard_db_url`,
  `vector_db_url`, `chord_proxy_url`, `github_token`), or
- a `crate::config::*` helper (e.g. `crate::config::atlas_database_url`,
  `crate::config::intake_database_url`) for values that legitimately need a
  single crate-wide accessor.

`src/config.rs` (the central `crate::config` accessor module),
`src/<secret-manager>/mod.rs` (the <secret-manager> bootstrap-credential reader — the one
place the crate's own bootstrap credential, `INFISICAL_CLIENT_ID`/
`INFISICAL_CLIENT_SECRET`, is read), and `src/secrets_bootstrap.rs` (the
startup fetch that materializes downstream `GITEA_*`/`PLANE_*`/`GITHUB_*`/
media-domain secrets into the process environment — see that file's module
doc) are exempt from Rule 1 entirely: they ARE the sanctioned
secret-materialization layer, not a call site of it.

**What counts as an `env::var` call.** The rule does not rely on the read
being spelled `std::env::var`. It matches all of:
- fully-qualified `std::env::var("NAME")`,
- `env::var("NAME")` (2-segment, e.g. via `use std::env;`), and
- a bare `var("NAME")` or aliased `getenv("NAME")` **when the same file has a
  `use std::env::var;` / `use std::env::var as getenv;` import** — a bare
  `var(...)` in a file WITHOUT such an import is treated as some other
  function and left alone (keeps false positives low).

**What counts as "inside `execute`".** The read is flagged if it is lexically
nested *anywhere* inside an `execute`/`execute_structured` body — including
inside a local helper fn or closure defined within `execute` — not only when
`execute` is the immediately-enclosing function. (Wrapping the raw read in a
nested helper does not evade the rule.) A read in a *sibling* impl method
(e.g. a `from_env()` next to `execute`) is the sanctioned accessor pattern
and is correctly NOT flagged.

**Test code is exempt, and `cfg(not(test))` is not test code.** Reads inside
`#[test]`/`#[cfg(test)]`/`#[cfg(all(test, …))]`/`#[cfg(any(test, …))]` items
are skipped (fixtures/mocks routinely set and read secret-shaped vars). The
checker PARSES the `cfg` predicate rather than substring-matching "test", so
production code guarded by `#[cfg(not(test))]` (or any `not(...)` wrapping
`test`) is fully checked, not wrongly skipped.

#### Why "inside `execute`", not "outside `src/config.rs`"

The CXEG-05 spec's grounding note said the sanctioned accessor is
`crate::config` and Rule 1 should flag any secret-shaped raw
`std::env::var` read outside `src/config.rs`. That does not match this
crate's actual, already-documented architecture. `crate::cortex`'s own module
doc says it plainly:

> This crate has no separate `SecretManager::get()` / `vault::manager()` API
> of its own — the runtime secret store is materialized into the process
> environment at deploy time, so a plain env read via `crate::config` (or...)
> already IS the sanctioned secret read.

And `secrets_bootstrap.rs`'s module doc documents the same design point: it
fetches from the runtime secret store and does `std::env::set_var` so "every
`X::from_env()`-style client built afterward transparently sees the current
value" — i.e. reading a materialized secret via `std::env::var` inside a
dedicated `from_env()`/accessor function is the crate's real, intentional,
decentralized pattern (mirrored by `crate::pki`'s bootstrap doc and
`scribe::graph::vec_embed`'s module doc). Centralizing every secret read
through the single file `src/config.rs` is not what this crate does anywhere
close to consistently — an audit while building this checker found **103**
pre-existing raw `std::env::var("SECRET_SHAPED_NAME")` reads outside
`src/config.rs`, essentially all of them inside `*::from_env()` constructors,
`get_pool()`/`pool()` Postgres-DSN helpers, or dedicated `fn foo_token()`
accessors — i.e. already correctly routed by the crate's real convention.
Flagging all of them would (a) not be a "real finding" in any actionable
sense — fixing 103 already-correct call sites into a literal `crate::config`
form would be pure churn with no behavior change — and (b) directly violates
the spec's own "must not retro-break the baseline" requirement.

The one genuine violation the audit *did* find — `TransitPlan::execute` in
`src/commute/mod.rs` reading `SF511_API_TOKEN` inline, instead of through a
dedicated accessor like every other tool in the crate — was fixed as part of
this item (extracted to `CommuteConfig::sf511_api_token()`), and a
confirming pass over every `execute`/`execute_structured` body in the
current tree found zero remaining occurrences. **Rule 1 is therefore scoped
to catch exactly that shape of mistake going forward: a secret read inlined
at the point of use inside a tool's `execute` body**, which is both the
precise real risk (a value that should be resolved once, in one accessor,
leaking into ad hoc call sites where it's easy to forget validation/masking)
and the only shape that's actually inconsistent with how this crate already
handles secrets everywhere else.

If a future change legitimately needs a genuinely new, one-off inline read
of a secret-shaped var directly in an `execute` body, it must be justified
with a `// house-style-allow: <reason>` waiver (see below) — the rule does
not auto-exempt anything based on file location beyond the three files
above.

#### Rule 1 classification

`secret_shaped(name)` (see `src/house_style/mod.rs`) splits the name on `_`
and flags it if any segment:
- is exactly `PAT` or `CREDS`, or
- ends with `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, or `JWT`,

or if the segments include both `DATABASE` and `URL` (a DB DSN carries
embedded credentials — CLAUDE.md explicitly enumerates "DB URLs" alongside
API keys/tokens/passwords as credentials).

This is deliberately **not** a blanket "name contains KEY/URL" substring
match. A bare `*_URL` suffix is NOT treated as secret-shaped on its own —
this crate has ~30 legitimate non-secret service-endpoint URLs
(`GITEA_URL`, `GITHUB_API_BASE`, `PLANE_API_URL`, `PROMETHEUS_URL`,
`JELLYSEERR_URL`, ...) that would all be false positives under a naive
"contains URL" rule (this is exactly the failure mode the spec asked Rule 1
to fix relative to the coarse Stage-4c grep). `PAT`/`CREDS` are matched as
whole segments, not substrings, so `SCRIBE_REPO_PATH`/`MODEL_REGISTRY_PATH`
(which contain `PAT` as a substring of `PATH`) are correctly NOT flagged.
See `secret_shaped_classifies_known_credentials` /
`secret_shaped_does_not_false_positive_on_non_secrets` in
`src/house_style/mod.rs` for the calibration tests.

**Known scope limits** (documented, not silently ignored):
- A `*_REDIS_URL` (e.g. `PLANE_REDIS_URL`) is not flagged even though a Redis
  URL can carry embedded auth — this crate keeps Redis auth in a separate
  `*_REDIS_PASSWORD` var everywhere it's used, so the URL itself doesn't
  currently carry credentials. If that ever changes, this rule needs a
  matching update (or the URL should get a `// house-style-allow:` waiver
  read note).
- Email addresses (`GOOGLE_LUMINA_EMAIL`, `GOOGLE_SECONDARY_EMAIL`) are PII,
  not "secret-shaped" per CLAUDE.md's credential categories, and are out of
  scope for this rule.

### Rule 2 — `RustTool::description()` must be non-empty

Every `impl RustTool for X` must have a `description()` that returns a
non-empty string. Checked for the common, dominant shape — a single string
literal as the function's tail expression:

```rust
fn description(&self) -> &str {
    "Does the thing."
}
```

**Limitation:** if `description()`'s body is anything other than a bare
literal tail expression (a `format!`, a `const` reference, a computed
value), the checker cannot verify non-emptiness statically and skips it —
no false positive, but also no coverage for that shape. Every
`description()` in the current tree uses the plain-literal shape, so this
limitation has zero practical impact today.

### Rule 3 — free-text tool args reaching a shell command

**Not implemented in this pass.** The spec allows scoping this rule down
(or deferring it) if it proves too noisy/heuristic-heavy; on triage, this
one requires real dataflow analysis (which of an `execute` body's several
locals feeds into a `Command::new`/shell invocation, whether it passed
through a length cap or a quoting helper first) to avoid drowning in false
positives on a syntactic AST pass — every existing shell-invoking tool
(`crate::dev`, the mirror engine, `pii_gate`) already documents its own
quoting/escaping discipline inline (see e.g. `crate::dev`'s module doc:
"User-supplied strings that must reach a shell are single-quoted..."), and
a naive "flag any `Command::new`/`format!` near a raw arg" heuristic would
flag most of them despite being correct. Deferred to a follow-up item scoped
specifically to this rule rather than shipped half-right here.

### Rule 4 — no `panic!` inside `RustTool::execute`/`execute_structured`

A tool must return a `ToolError` (the most specific variant —
`InvalidArgument`, `NotConfigured`, `Execution`, etc.) on unexpected or
malformed external input, never abort the process with `panic!`. Checked via
the same `execute`/`execute_structured`-body scoping as Rule 1, skipping
`#[cfg(test)]`/`#[test]` code.

**`.unwrap()` was evaluated and intentionally left out of this rule.** An
audit of the current tree found only 3 `.unwrap()` calls anywhere inside an
`execute`/`execute_structured` body, and none of them are the failure mode
this rule exists to catch:

- `src/council/mod.rs:311,401` — `self.store.sessions.lock().unwrap()`: a
  `Mutex::lock().unwrap()`, which panics only on lock poisoning (a prior
  panic elsewhere already corrupted shared state), not on external tool
  input. This is the idiomatic, crate-wide way this code handles
  `std::sync::Mutex`.
- `src/ledger/mod.rs:265` — `args["notes"].as_str().unwrap()`, guarded by an
  `if args["notes"].is_string()` check on the line directly above.

Distinguishing "unwrap on unvalidated external input" from "unwrap after an
equivalent guard" or "unwrap on a lock that only panics on poisoning"
requires real dataflow analysis, not a syntactic AST pass — attempting it
here would produce exactly these 3 false positives on the current, correct
tree with no way to calibrate around them without also hiding genuine
future violations. `panic!` was kept in the rule because it has a clean,
unconditional semantics (no legitimate "guarded panic!" idiom exists) and
zero current occurrences in any `execute`/`execute_structured` body, so
enforcing it retro-breaks nothing.

## The waiver convention

A `// house-style-allow: <reason>` line comment — on the same line as the
flagged code, or the line immediately above it — suppresses that one
finding. This mirrors this crate's existing `// pii-test-fixture` line-exact
convention (`crate::github::pii`), with one addition: **the reason is
mandatory**. `// house-style-allow` with no colon, or with an empty reason
after the colon, does NOT suppress anything — it is itself reported as a
`house-style-waiver-reason` violation (the original finding is also included
in that violation's message, so it stays visible; the waiver can never
silently swallow a finding).

```rust
// house-style-allow: legacy fixture path, tracked in TERM-123, not a real secret
let x = std::env::var("SOME_TOKEN_LOOKALIKE");
```

## Allow-list

| Scope | Why |
| --- | --- |
| `src/config.rs` | The central `crate::config` accessor module — the file IS the sanctioned accessor, not a caller of it. |
| `src/<secret-manager>/mod.rs` | Reads the crate's own bootstrap credential (`INFISICAL_CLIENT_ID`/`INFISICAL_CLIENT_SECRET`) — this is the <secret-manager> client itself. |
| `src/secrets_bootstrap.rs` | The startup fetch that materializes downstream secrets into the process environment (`std::env::set_var`) for every `*::from_env()` constructor to read afterward. |
| Any code inside `#[cfg(test)]` / `#[test]` / `#[tokio::test]` | Test fixtures, mocks, and env-snapshot/restore helpers (e.g. `clear_github_credential_env`) routinely set/read/clear secret-shaped env vars to drive test scenarios — not production secret handling. |
| Any raw `std::env::var` read OUTSIDE a `RustTool::execute`/`execute_structured` body | Covered by Rule 1's scope decision above — this is the crate's established `from_env()`/dedicated-accessor pattern, not a violation. |

No additional per-name allow-list entries were needed: the segment-aware
classification in [Rule 1 classification](#rule-1-classification) already
avoids the false positives a blanket substring match would have produced
(the `*_URL`/`*PATH` cases above), so nothing needed a name-specific carve-out
on top of it.

## Deny-by-default

The checker is a source-tree gate, so it fails **closed**: any `src/**/*.rs`
file that cannot be walked, read, or parsed by `syn` is reported as a
`house-style-file-error` violation (which fails `cargo test`/the gate), never
silently skipped. A file the checker can't inspect could be hiding real
violations, so "couldn't look at it" must be a failure, not a pass. (In
practice `cargo build`/`cargo test` would also fail on a genuinely
un-parseable file, but the gate stands on its own rather than assuming an
upstream step already caught it.)

## Known limitations

These are deliberate scope boundaries of a *mechanical* lint, documented so
nobody mistakes a green result for a stronger guarantee than it gives:

- **Rule 1 matches string-literal var names only.** A read whose key is
  indirected — a `const KEY: &str = "…"; env::var(KEY)`, a `let`-bound name,
  or a `format!("{prefix}_TOKEN")`-built key — is **not** resolved or
  classified. Statically deciding whether such a computed value is
  secret-shaped needs real dataflow/const-evaluation analysis, which is out
  of scope for an AST-shape checker. This is the one class of Rule-1 blind
  spot; it is accepted, not accidental. (The crate's actual reads are all
  string literals today, so this doesn't hide anything currently — but a
  future indirected read would slip past, so reviewers should still treat
  "reads a secret inline in `execute`" as a code-review item, not something
  the lint fully guarantees against.)
- **Rule 2** only verifies a `description()` whose body is a single string
  literal (the shape every tool uses today); a computed description is
  skipped (see Rule 2 above).
- **Rule 3** is unimplemented (see Rule 3 above).
- **Rule 4** covers `panic!` but intentionally not `.unwrap()` (see Rule 4
  above).

## Why not dylint

The spec mentions `dylint` as the reference implementation, but a full
dylint/rustc-driver custom-lint crate needs a pinned nightly toolchain and
compiles against unstable rustc internals — too fragile for this build host
(and for CI generally: a rustc-internals-API break on a routine nightly bump
would silently stop enforcing house style until someone noticed and
re-pinned). This checker is the spec's explicitly-allowed "equivalent": a
deterministic `syn`-AST checker running on the normal stable toolchain, as
an ordinary `#[test]` in the Stage-4 gate.
