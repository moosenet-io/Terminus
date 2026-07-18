## Mesh: federating multiple upstream Terminus servers

Beyond the single personal-registry upstream `terminus-primary` federates by
default, Terminus can federate an arbitrary set of upstream Terminus-shaped
MCP servers through a config-driven **mesh registry** (`crate::mesh`). Rather
than a hard-coded client per backend, each upstream is declared as data and
validated at startup.

Configuration is entirely non-secret and environment-driven (structural
config only — credentials are never inlined):

| Variable | Meaning |
| --- | --- |
| `TERMINUS_MESH_ENABLED` | Master switch. Truthy (`1`/`true`/`yes`/`on`, case-insensitive) enables the mesh; anything else (including unset) leaves it dormant — an empty registry, never an error. |
| `TERMINUS_MESH_UPSTREAMS_JSON` | A JSON array of upstream entries (see below). Unset/blank while enabled is a dormant no-op; malformed while enabled is a clear startup error naming the offending field. |

Each entry in the JSON array declares:

| Field | Meaning |
| --- | --- |
| `name` | Stable, unique identifier for the upstream. |
| `url` | Reachable base URL (must be non-empty). |
| `transport` | `"mtls"` or `"bearer"` (case-insensitive). |
| `namespace` | Unique prefix its federated tools are namespaced under; must match `^[a-z0-9]{2,16}$`. |
| `secret_key` | **NAME only** of the credential in the runtime secret store (for `bearer`); omit for pure-mTLS upstreams. Never an inline token value. |
| `enabled` | Optional bool, default `true`. A `false` entry is parsed/validated but excluded from dialing. |

```json
[
  { "name": "personal", "url": "https://personal.example.internal:8443",
    "transport": "mtls", "namespace": "personal" },
  { "name": "fleet-b", "url": "https://fleet-b.example.internal:8443",
    "transport": "bearer", "namespace": "fleetb",
    "secret_key": "TERMINUS_MESH_FLEETB_TOKEN", "enabled": false }
]
```

Credentials are referenced by secret-key **name** only and resolved lazily,
right before a dial — never at registry-load time, and never stored as a value
on the registry — following the same "materialized into the process
environment at startup, plain env read afterward IS the secret read"
convention the rest of the crate uses (see `crate::pki`). Registry loading,
validation, and inspection perform zero secret-store reads.

