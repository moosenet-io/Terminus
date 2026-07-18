## Architecture

```mermaid
flowchart LR
    Clients["MCP clients<br/>(stdio / HTTP+mTLS)"] --> Gateway

    subgraph Gateway["terminus-primary (gateway)"]
        Dispatch["Dispatch +<br/>JSON-Schema validation"]
        Gov["Governance:<br/>path-jail, vault secrets,<br/>PII gate, audit log"]
        Dispatch --> Gov
    end

    Gateway --> Core
    Gateway --> Personal
    Gateway --> Chord

    subgraph Core["Core tool registry (local)"]
        CoreTools["~52 domain tool modules<br/>(git forges, trackers, infra, ...)"]
    end

    subgraph Personal["terminus_personal (federated)"]
        PersonalTools["Personal-registry tools"]
    end

    subgraph MeshGroup["Mesh (optional, N upstreams)"]
        Upstream["Federated Terminus-shaped<br/>MCP servers"]
    end

    Personal -. "optional" .-> MeshGroup

    Chord["Chord<br/>(inference proxy)"]
```

MCP clients connect over stdio or HTTP/mTLS transports to the Terminus core
MCP server, which owns dispatch, JSON-Schema validation, and governance.
Governance is mandatory and layered: a path-jailed filesystem, vault-only
secret access (no raw environment reads for secrets), a PII gate, and a
sanitized audit log — tools are read-only by default, write scopes are
explicit. Behind the registry sit the 52 domain tool modules, each owning its
own typed client and credentials. See
[`docs/architecture/`](docs/architecture/) for the federation, auth, and
Chord-integration deep-dives.

