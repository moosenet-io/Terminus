[← Plane overview](README.md) · [← Tool reference](../../README.md)

# Plane — metadata and identity

This page covers the 6 read/query tools over per-project metadata (states, labels, members,
comments) plus the 2 multi-identity introspection tools (`plane_whoami`,
`plane_list_identities`). See [Multi-identity](README.md#multi-identity) on the overview page for
the full identity-resolution design these two tools sit on top of.

Types: `State`, `Label`, `Member`/`MemberDetail`, `Comment` (`src/plane/types.rs:56-101, 143-156`).

## Table of contents

- [plane_list_states](#plane_list_states)
- [plane_list_labels](#plane_list_labels)
- [plane_list_members](#plane_list_members)
- [plane_list_comments](#plane_list_comments)
- [plane_create_comment](#plane_create_comment)
- [plane_whoami](#plane_whoami)
- [plane_list_identities](#plane_list_identities)

## plane_list_states

`mod.rs:2044-2084`. Lists a project's workflow states.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `identity` | string | no | See [Multi-identity](README.md#multi-identity) |

**Behavior**: GET `projects/{project_id}/states/` (cached), parse `ApiList<State>`. Each `State`
carries a `group` (`backlog`/`unstarted`/`started`/`completed`/`cancelled`) — this is the field
`plane_list_issues_by_state` and `plane_close_work_item` filter/search on (see
[work-items.md](work-items.md)).

**Output shape**: `"No states found"` if empty, else:
```
States (N):
  [<uuid>] <name> (group: <group>, color: <color>)
  ...
```

## plane_list_labels

`mod.rs:2088-2129`. Lists a project's labels.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/labels/` (cached), parse `ApiList<Label>`.

**Output shape**: `"No labels found"` if empty, else:
```
Labels (N):
  [<uuid>] <name> (color: <color|->)
  ...
```

## plane_list_members

`mod.rs:2133-2176`. Lists a project's members.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/members/` (cached), parse `ApiList<Member>`. `Member.role`
is a raw `u8` role code from Plane — this tool does not translate it to a human label.

**Output shape**: `"No members found"` if empty, else:
```
Members (N):
  [<uuid>] <display_name|unknown> (role: <role>)
  ...
```

**Note**: results can vary by the calling identity's role/permissions — another reason the GET
cache key includes the active token (see
[GET cache](README.md#get-cache-in-process-optional-shared-redis)).

## plane_list_comments

`mod.rs:2180-2228`. Lists comments on a work item.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `issue_id` | string | yes | Issue UUID |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/issues/{issue_id}/comments/` (cached), parse
`ApiList<Comment>`. Prefers `comment_stripped` (plain text) over `comment_html`, falling back to
`"(empty)"` if neither is present.

**Output shape**: `"No comments on this issue"` if empty, else:
```
Comments (N):
  [<uuid>] <author|unknown>: <text>
  ...
```

## plane_create_comment

`mod.rs:2232-2269`. Adds a comment to a work item.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `issue_id` | string | yes | Issue UUID |
| `comment` | string | yes | Comment text |
| `identity` | string | no | |

**Behavior**: POSTs `{"comment_html": "<p><comment></p>"}` to
`projects/{project_id}/issues/{issue_id}/comments/` — the plain `comment` text is wrapped in a
single `<p>` tag with **no HTML-escaping** of its contents (a caller passing raw `<`/`>`/`&` gets
that markup interpreted by Plane's HTML renderer, not literal text).

**Output shape**: `"Comment added (ID: <uuid>)"`.

## plane_whoami

`mod.rs:2712-2797`. Reports which identity is active, or verifies a token is currently accepted.

**Input schema**

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `identity` | string | no | active default | Identity to report/verify (does **not** use `with_identity_param` — this tool defines `identity` itself since its semantics differ slightly: see below) |
| `verify` | boolean | no | `false` | When true, makes a real authenticated Plane read to prove the selected token's current validity |

**Behavior — two distinct modes:**

1. **`verify: false` (default) — config-only, no network call.**
   - With an explicit `identity`: reports whether that name is configured — checking both the
     named-identities map *and* whether it equals the currently active default (so the active
     default is reported "configured" even if, unusually, it isn't literally present as its own
     `PLANE_PAT_<NAME>` entry). Unknown name → `ToolError::NotFound`.
   - With no `identity`: requires `client.configured()` (else `NotConfigured`), then reports the
     resolved active-default name, or an explanatory string if a default token is set but no name
     could be resolved for it.
2. **`verify: true` — a real authenticated read.** Resolves the selected identity (explicit
   `identity`, else active default) via the shared `resolve_identity`, then performs one GET
   against `projects/` — **bypassing the GET cache** specifically so a recently-cached success
   can't mask an expired token (`mod.rs:2751-2752`). The response body is discarded; only the HTTP
   status is inspected:
   - 2xx → `"token VALID (authenticated read succeeded, 200)."`
   - 401/403 → `"token REJECTED (Plane returned <status> — the token is likely expired or
     revoked)."`
   - anything else (5xx, 422, …) → the real `ToolError` is propagated, **not** mislabeled as
     REJECTED — discrimination is on the literal HTTP status, never on error-message text.

**Output shape**: a single human-readable sentence, one of the four forms above.

**Errors**: `NotFound` for an unknown identity name (non-verify path); `InvalidArgument` for an
unknown identity name when `verify: true` (from `resolve_identity` → `for_identity`);
`NotConfigured` if no default is configured; the real `ToolError` for a non-auth failure during
`verify: true`.

**Never leaks a token value** in any output path — this is exercised directly by
`test_plane_whoami_verify_never_leaks_token_value` (`mod.rs:4463-4476`).

**Example**
```json
{"identity": "vigil", "verify": true}
```
```
Plane identity 'vigil': token REJECTED (Plane returned 403 Forbidden — the token is likely expired or revoked).
```

## plane_list_identities

`mod.rs:2804-2851`. Lists every configured identity's name.

**Input schema**: `{}` — no arguments at all (this tool does not itself need to select an
identity).

**Behavior**: reads `client.identity_names()` (sorted, lowercased) — derived from the client's
already-scanned `identities` map (populated once at process start), never a fresh env re-scan, so
the reported list is guaranteed to be exactly what `for_identity()` can resolve. Also reports the
active default's name, if resolved.

**Output shape** (JSON, unlike most other tools in this module which return plain text):
```json
{
  "identities": ["axon", "claude", "harmony", "vigil"],
  "count": 4,
  "active_default": "lumina",
  "prefix": "PLANE_PAT_",
  "note": "..."   // only present when count == 0
}
```

When `count == 0`, a `note` field explains whether a default `PLANE_API_KEY` identity exists (no
named identities, just the unsuffixed default) or nothing is configured at all — either way
pointing at the `PLANE_PAT_<NAME>` provisioning convention.

**Never returns token values** — names only, matching `plane_whoami`'s safety posture. Per its own
description: "Use the identity matching who should act on an item rather than always your own."
