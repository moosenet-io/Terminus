[← Plane overview](README.md) · [← Tool reference](../../README.md)

# Plane — projects, cycles, and modules

This page covers the 13 tools over Plane's three container concepts: **projects** (the top-level
container, resolved via the [UUID gotcha](README.md#the-project-id-uuid-gotcha) on every other
tool), **cycles** (sprints — read-only in this module), and **modules** (a lighter-weight grouping
that supports full CRUD plus explicit issue-membership tools). Types: `Project`, `Cycle`, `Module`
(`src/plane/types.rs:5-18, 103-141`).

Every tool here shares the machinery on the [overview page](README.md): the optional `identity`
argument, `project_id` accepting a UUID or identifier, rate-limited/cached GETs, and uniform
HTTP-status-to-`ToolError` mapping.

## Table of contents

- [plane_list_projects](#plane_list_projects)
- [plane_get_project](#plane_get_project)
- [plane_list_cycles](#plane_list_cycles)
- [plane_get_cycle](#plane_get_cycle)
- [plane_list_cycle_issues](#plane_list_cycle_issues)
- [plane_list_modules](#plane_list_modules)
- [plane_get_module](#plane_get_module)
- [plane_create_module](#plane_create_module)
- [plane_update_module](#plane_update_module)
- [plane_delete_module](#plane_delete_module)
- [plane_list_module_issues](#plane_list_module_issues)
- [plane_add_issue_to_module](#plane_add_issue_to_module)
- [plane_remove_issue_from_module](#plane_remove_issue_from_module)

## plane_list_projects

`mod.rs:1168-1201`. Lists every project in the workspace.

**Input schema**: `{}` plus the shared `identity` field — no other arguments.

**Behavior**: GET `workspaces/{workspace}/projects/` (cached), parse `ApiList<Project>`. This is
also the exact endpoint `resolve_project_id` calls internally to resolve a non-UUID `project_id`
on every other tool, so it benefits from (and populates) the same GET cache.

**Output shape**: `"No projects found in workspace"` if empty, else:
```
Found N project(s):
  [<uuid>] <name> (<identifier>)
  ...
```

**Note**: which projects are returned depends on the calling identity's Plane user membership —
this is why the GET cache key includes the active token (see
[GET cache](README.md#get-cache-in-process-optional-shared-redis)).

## plane_get_project

`mod.rs:1205-1239`. Fetches one project's detail.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier (e.g. `"LM"`) |
| `identity` | string | no | See [Multi-identity](README.md#multi-identity) |

**Behavior**: resolves `project_id` to a UUID, then GET `projects/{id}/` (cached).

**Output shape**:
```
Project: <name>
ID: <uuid>
Identifier: <identifier>
Description: <desc|(none)>
```

**Errors**: `NotFound` if `project_id` doesn't resolve or Plane returns 404.

## plane_list_cycles

`mod.rs:1552-1595`. Lists cycles (sprints) in a project. **Read-only** — this module has no
create/update/delete for cycles.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/cycles/` (cached), parse `ApiList<Cycle>`.

**Output shape**: `"No cycles found"` if empty, else:
```
Found N cycle(s):
  [<uuid>] <name> (<status|unknown>) <start|-..<end|->
  ...
```

## plane_get_cycle

`mod.rs:1599-1638`. Fetches one cycle's detail.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `cycle_id` | string | yes | Cycle UUID |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/cycles/{cycle_id}/` (cached).

**Output shape**:
```
Cycle: <name>
ID: <uuid>
Status: <status|unknown>
Dates: <start|-> to <end|->
```

## plane_list_cycle_issues

`mod.rs:1642-1683`. Lists the issues assigned to a specific cycle.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `cycle_id` | string | yes | Cycle UUID |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/cycles/{cycle_id}/cycle-issues/` (cached), parse
`ApiList<Issue>`.

**Output shape**: `"No issues in this cycle"` if empty, else `Cycle issues (N):` followed by
`  [<uuid>] <name>` lines.

## plane_list_modules

`mod.rs:1687-1728`. Lists modules in a project.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/modules/` (cached), parse `ApiList<Module>`.

**Output shape**: `"No modules found"` if empty, else:
```
Found N module(s):
  [<uuid>] <name> (<status|unknown>)
  ...
```

## plane_get_module

`mod.rs:1732-1771`. Fetches one module's detail.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `module_id` | string | yes | Module UUID |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/modules/{module_id}/` (cached).

**Output shape**:
```
Module: <name>
ID: <uuid>
Status: <status|unknown>
Dates: <start|-> to <target|->
```

## plane_create_module

`mod.rs:1775-1819`. Creates a module.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `name` | string | yes | Module name |
| `description` | string | no | |
| `status` | string | no | e.g. `backlog`/`planned`/`in-progress`/`paused`/`completed`/`cancelled` |
| `start_date` | string | no | `YYYY-MM-DD` |
| `target_date` | string | no | `YYYY-MM-DD` |
| `identity` | string | no | |

**Behavior**: POST `projects/{project_id}/modules/` with only the fields present in `args`.

**Output shape**: `"Created module: <name> (ID: <uuid>)"`.

**Errors**: `InvalidArgument` for missing `name`; shared HTTP error table otherwise.

## plane_update_module

`mod.rs:1868-1916`. Patches fields on an existing module.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `module_id` | string | yes | Module UUID to update |
| `name` | string | no | |
| `description` | string | no | |
| `status` | string | no | |
| `start_date` | string | no | |
| `target_date` | string | no | |
| `identity` | string | no | |

**Behavior**: builds a PATCH body from whichever fields are present; at least one is required.

**Output shape**: `"Updated module: <name> (ID: <uuid>)"`.

**Errors**: `InvalidArgument` ("No fields to update provided") if no field is given.

## plane_delete_module

`mod.rs:1920-1952`. Permanently deletes a module.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `module_id` | string | yes | Module UUID to delete |
| `identity` | string | no | |

**Behavior**: DELETE `projects/{project_id}/modules/{module_id}/`. Per its own description, this
does **not** delete the issues that were in the module — only the module container and its
membership links.

**Output shape**: `"Deleted module <module_id>"`.

## plane_list_module_issues

`mod.rs:1823-1864`. Lists the issues linked to a module.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `module_id` | string | yes | Module UUID |
| `identity` | string | no | |

**Behavior**: GET `projects/{project_id}/modules/{module_id}/module-issues/` (cached), parse
`ApiList<Issue>`.

**Output shape**: `"No issues in this module"` if empty, else `Module issues (N):` followed by
`  [<uuid>] <name>` lines.

## plane_add_issue_to_module

`mod.rs:1956-2002`. Adds one or more issues to a module's membership.

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `module_id` | string | yes | Module UUID |
| `issue_id` | string | no\* | A single issue UUID to add |
| `issue_ids` | array\<string\> | no\* | Multiple issue UUIDs to add |
| `identity` | string | no | |

\* At least one of `issue_id`/`issue_ids` must yield a non-empty id list.

**Behavior**: accepts either or both forms — `issue_id` and any entries in `issue_ids` are
collected into one list (`mod.rs:1983-1993`; no de-duplication is performed if both happen to
name the same id) — then POSTs once to the `module-issues` endpoint via the shared
`PlaneClient::add_issues_to_module` helper (`mod.rs:923-945`, body `{"issues": [<uuid>, ...]}`).
This is the same helper `plane_create_work_item`/`plane_update_work_item`'s optional `module_id`
field uses internally, so link semantics live in one place.

**Output shape**: `"Added N issue(s) to module <module_id>"`.

**Errors**: `InvalidArgument` ("Provide issue_id or a non-empty issue_ids array") if neither
yields an id.

## plane_remove_issue_from_module

`mod.rs:2006-2040`. Removes one issue from a module (the issue itself is untouched, still in the
project).

**Input schema**

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `project_id` | string | yes | UUID or identifier |
| `module_id` | string | yes | Module UUID |
| `issue_id` | string | yes | Issue UUID to remove from the module |
| `identity` | string | no | |

**Behavior**: DELETE `projects/{project_id}/modules/{module_id}/module-issues/{issue_id}/`.

**Output shape**: `"Removed issue <issue_id> from module <module_id>"`.
