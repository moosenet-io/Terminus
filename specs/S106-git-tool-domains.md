# Git Tool Domains — git-public + git-private (provider-agnostic forge tools)
plane_project: TERM
module: Terminus
prefix: GITX
spec_id: S106-git-tool-domains

<!-- HOUSEKEEPING (Claude): operator draft was labeled spec_id S96 / "v3.5 pipeline". S96 is
     already taken this session (S96-terminus-personal-runtime-secret-fetch / PSEC). Re-id'd to
     S106; execute under the CURRENT build pipeline v3.9. Verify the GITX prefix is free via
     plane_prefix_check at ingest. Reconciliation with existing code — see "Integration with
     already-shipped work" at the bottom. -->

## Metadata
- **Author:** the operator (Moose)
- **Session:** S106
- **Date:** 2026-07-08
- **Module version:** Terminus (terminus-rs infra layer)
- **Estimated total:** ~40h (provider adapters + shared surface + governance)
- **Context:** Overhaul the MCP git tools from provider-specific (a "GitHub tool", a "Gitea tool") into two
  provider-AGNOSTIC domains that share one comprehensive endpoint surface and differ only by provider pool +
  governance posture:
  - **git-private** — source-of-truth forges (self-hosted). Replaces the Gitea tool. Full operator R/W. Canonical code.
  - **git-public** — public/mirror forges (outbound). Replaces the GitHub tool. Same endpoint breadth, but the
    **exfiltration surface** — the PII gate is load-bearing on every write.
  Both expose the SAME endpoint vocabulary (a forge is a forge); the split is provider pool + posture, not
  capability. Providers are pluggable adapters behind a common trait.

  **Design principle — one surface, two pools, two postures:**
  - One surface: identical endpoint set on both tools (repos/branches/PRs/issues/releases/webhooks/packages/etc.).
  - Two pools: git-private → self-hosted forges; git-public → hosted/public forges.
  - Two postures: git-private = operator source-of-truth (full R/W, vault creds); git-public = published mirror
    (PII gate hard-block on writes; a push that fails the sweep is withheld).

## Pre-flight
- Repository: `moosenet/Terminus` (terminus-rs). Dev-box git transport is HTTPS as moose (live).
- Vault/<secret-manager> secrets, per-provider, never literals: self-hosted — GITEA_URL + GITEA_PAT_<NAME> (S105 model),
  FORGEJO_URL + FORGEJO_TOKEN; public — GITHUB_TOKEN (→ GITHUB_PAT_<NAME> per-identity), CODEBERG_TOKEN,
  GITLAB_TOKEN, etc. Only configured providers activate.
- PII gate implementation available (reuse GHMR-01's Rust `src/github/pii.rs` sweep engine); unconditional on git-public writes.
- Plane via the Terminus Plane tool; ingest project TERM (Plane identity for dev-box actions = claude).
- Baseline: record terminus-rs `cargo test --workspace` count; never regress.

## The shared endpoint surface (BOTH tools expose this)
Repos: list/get/create/update/delete/fork/mirror-config/visibility/metadata · Branches/refs:
list/get/create/delete/protection/default-branch · Commits: list/get/compare-diff/status · Pull/merge
requests: list/get/create/update/review/comment/merge/close · Issues: list/get/create/update/comment/label/
assign/close · Releases/tags: list/get/create/update/delete/assets · Webhooks: list/create/update/delete/test
· Packages/registry: list/get/publish/delete · Content: read/write file, list tree, raw fetch · Org/collab:
members/teams/permissions · **Capability introspection:** each adapter advertises which endpoints it supports;
the tool reports "unsupported by provider X" instead of erroring. Vocabulary constant; availability varies.

## Provider list
git-private (self-hosted): Gitea (`gitea`, Gitea REST v1 — CURRENT source-of-truth, the existing gitea tool
becomes this adapter; cargo registry, git-over-SSH :2222), Forgejo (`forgejo`, Gitea-compatible — reuse client),
GitLab CE (`gitlab_ce`, v4, optional), Gogs (`gogs`, optional/minimal), OneDev (`onedev`, optional).
git-public (hosted): GitHub (`github`, REST v3/GraphQL — CURRENT mirror, existing github tool becomes this adapter),
Codeberg (`codeberg`, Forgejo API — RECOMMENDED public target: non-profit/EU/no-AI-training/Forgejo-lineage, reuse
Gitea client), GitLab SaaS (`gitlab_saas`, v4 — share client with gitlab_ce), Bitbucket (`bitbucket`, Cloud REST 2.0,
optional), SourceHut (`sourcehut`, REST+GraphQL — NO web-PR/registry, reduced capability set, optional), Radicle
(`radicle`, p2p — experimental/future).
Adapter reuse: Gitea/Forgejo/Codeberg = ONE Gitea-compatible client; gitlab_ce/gitlab_saas = ONE v4 client.

## Governance postures
git-private: full operator R/W (source-of-truth); per-provider vault creds; writes audit-logged; destructive ops
(repo delete, force-push, history rewrite) require confirmation, force-push/history-rewrite human-gated.
git-public (EXFILTRATION SURFACE): PII gate is an UNCONDITIONAL hard block on every write/push/publish — a failing
sweep WITHHOLDS the push (stays private), logs + flags; no bypass, no cadence fast-path. Reads unrestricted.
Respect ISO egress isolation (per-provider host allowlist). First-publish to any public provider is human-gated
(mirror_activated model — confirmed once per repo/provider).

## Build items (sequenced)
### GITX-01: Common ForgeProvider trait + capability-introspection framework — High, claude, 6h
The `ForgeProvider` trait defining the shared endpoint surface + capability-advertisement (each adapter declares
supported endpoints). Both tools built on it. No adapters yet. AC: trait covers full vocabulary; capability
introspection returns per-adapter support map; unsupported → clean "unsupported by provider" (negative test); no
literals; secrets via vault/SecretManager; README; tests green.

### GITX-02: Gitea-family adapter (Gitea + Forgejo + Codeberg — one Gitea-compatible client) — Critical, claude, 7h
One client (base-URL + creds parameterized) serving Gitea + Forgejo (private) + Codeberg (public). Refactor the
CURRENT gitea tool (S105 GITEA_PAT_<NAME> multi-identity, gitea_cargo_publish, git-relay) INTO this adapter. Full
capability set. AC: all shared endpoints vs Gitea API; same client drives all three by config; capability map;
creds via vault (GITEA_PAT_<NAME>); negative test for unreachable instance; preserves S105 identity model.

### GITX-03: GitHub adapter (git-public) — Critical, claude, 6h
GitHub REST v3 (+GraphQL where needed). Refactor the CURRENT github tool's logic INTO this adapter. Full
capability set. AC: shared endpoints vs GitHub API; capability map; per-identity creds via vault; egress
isolation; negative test for auth/scope failure.

### GITX-04: GitLab adapter (v4 — gitlab_ce + gitlab_saas) — Medium, claude, 6h
GitLab v4 client serving self-hosted + SaaS by config; MR↔PR / project↔repo mapping to the common surface. AC:
shared endpoints vs v4; terminology mapped; both by config; capability map; creds via vault.

### GITX-05: git-private + git-public tool assembly + provider routing + posture enforcement — Critical, claude, 7h
Assemble the two MCP tools on the trait: git-private → self-hosted pool, git-public → public pool. ENFORCE
postures: git-public writes → unconditional PII gate (reuse GHMR sweep) + first-publish human gate + egress
isolation; git-private destructive/history-rewrite → confirmation/human-gated. **Integrate the GHMR mirror
engine here** as git-public's swept-clean-tree write path (mirror = git-private source → PII-gated git-public
push). AC: git-private full R/W; git-public write → PII hard-block (withhold on fail, negative test) +
first-publish gate; egress isolation; provider selection by config; no literals; secrets via vault.

### GITX-06: Optional/experimental adapter stubs + capability maps (Bitbucket, SourceHut, Gogs, OneDev, Radicle) — Low, claude, 4h
Stubs + honest capability advertisements so the tool KNOWS the providers + reports reduced surfaces. SourceHut =
no web-PR/registry; Radicle = experimental. AC: accurate capability map per stub; unimplemented endpoint → clean
"not yet implemented for provider X"; no false capability claims.

### GITX-07: Documentation — Medium, gemini, 4h
Document the two domains, shared surface, provider manifest, capability introspection, the two postures, and the
operator "how to add/activate a provider" (vault creds + config). Update README + moosenet-spec skill.

## Integration with already-shipped work (Claude reconciliation)
- Existing **github tool** (core) → refactored into the **git-public `github` adapter** (GITX-03).
- Existing **gitea tool + S105/GPAT** (GITEA_PAT_<NAME>, gitea_cargo_publish, relay) → refactored into the
  **git-private `gitea` adapter** (GITX-02); the identity model + relay carry forward.
- **GHMR mirror engine** (GHMR-01..06, just merged — Rust PII sweep + clean work-dir + mirror-approved tag +
  bounded subagent cleaning) is NOT rebuilt — it becomes the swept-clean-tree + PII engine that git-public's
  write posture (GITX-05) uses to route a mirror to a chosen public provider. `.moosenet-pipeline.yaml` gains a
  provider/target selector.
- git-public = CORE tool (Chord-embedded); git-private = personal tool (terminus_personal) per the taxonomy —
  confirm placement at assembly.
- #7 Chord rebuild now happens against THIS overhaul (not the interim github-only build). terminus-rs version
  bump accordingly.
