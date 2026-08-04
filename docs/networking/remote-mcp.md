# The public MCP connector edge (RMCP-09)

The other two documents in this directory ([WireGuard](wireguard.md),
[Tailscale](tailscale.md)) describe how to get *your own* machines onto a
private path to Terminus. This one is the opposite problem: how to let a hosted
third party — Anthropic's Claude surfaces — reach one small, deliberately
chosen part of Terminus over the public internet, without putting anything else
within reach.

## The shape of the thing

```
          internet
              │  TLS
              ▼
   ┌────────────────────────┐   443, publicly resolvable hostname
   │  reverse proxy         │   terminates TLS, appends X-Forwarded-For
   └───────────┬────────────┘
               │  plain HTTP, private interface
               ▼
   ┌────────────────────────┐   RMCP_EDGE_BIND:RMCP_EDGE_PORT
   │  edge listener         │   per-path source policy  (src/oauth/edge.rs)
   │  inside terminus-primary│
   └───────────┬────────────┘
               │
               ▼
    the same router the private listeners serve
```

The edge is a **fourth listener inside `terminus-primary`**, not a separate
service. It serves the same router as the plain, mTLS and tailnet listeners —
what makes it safe to expose is that every request through it passes
`oauth::edge`'s policy first, and that policy admits only a fixed, tiny set of
paths.

## What the edge exposes, and nothing else

| Path | Source class |
|---|---|
| `/.well-known/oauth-protected-resource` (and its path-suffixed form) | `anthropic` |
| `/.well-known/oauth-authorization-server` | `anthropic` |
| `/mcp` | `anthropic` |
| `/oauth/token` | `anthropic` (`interactive` under the `claude-code` profile) |
| `/oauth/register` | `anthropic` |
| `/oauth/revoke` | `anthropic` |
| `/oauth/authorize` | `interactive` |
| `/oauth/login`, `/oauth/consent` | `interactive` |

Everything else the router serves — `/enroll`, `/admin/*`, `/healthz`, the
inference routes, every tool-dispatch path other than `/mcp` — is **not
reachable from the edge at all**. A path with no entry in the table above is
denied; there is no "default open" case anywhere in the policy.

## Why the policy is per-PATH

This is the part that is easy to get wrong, and the failure is silent.

The two halves of the OAuth flow **arrive from different networks**:

- Anthropic's infrastructure fetches the `.well-known` documents, calls
  `/oauth/token` and `/oauth/register`, and talks to `/mcp`. All of that comes
  from Anthropic's published **outbound** egress range.
- `/oauth/authorize`, and the login and consent forms behind it, open **in your
  own browser**. They arrive from your network, and never from Anthropic.

So the intuitive control — "firewall this hostname to Anthropic's range" —
produces a connector that discovers correctly, gets a `401` correctly, and then
shows the person trying to approve it a blank 403. The symptom points at the
authorization server; the cause is a firewall rule. Hence two classes.

Claude Code is a third case: it runs the *entire* flow from the user's own
machine with an RFC 8252 loopback redirect, so `/oauth/token` also arrives from
your network there. That is the `claude-code` profile, and it is opt-in rather
than the default because moving the token endpoint into the interactive class is
a real (if small) widening.

## Configuration

All of it is environment configuration, listed by name in `.env.example`. None
of it is compiled in — including the source ranges, so a published range that
changes is an env edit and a restart, not a release.

| Variable | Meaning |
|---|---|
| `RMCP_EDGE_ENABLED` | Master switch. Unset ⇒ no edge listener, nothing parsed. |
| `RMCP_EDGE_BIND` / `RMCP_EDGE_PORT` | Private interface the edge binds. Defaults to loopback. |
| `RMCP_EDGE_PROFILE` | `anthropic-hosted` (default) or `claude-code`. |
| `RMCP_EDGE_POLICY_JSON` | Optional JSON object replacing the profile's path→class table outright. |
| `RMCP_EDGE_ANTHROPIC_CIDRS` | Comma-separated CIDRs for the `anthropic` class. |
| `RMCP_EDGE_INTERACTIVE_CIDRS` | Comma-separated CIDRs for the `interactive` class. |
| `RMCP_EDGE_TRUSTED_PROXIES` | Comma-separated CIDRs whose `X-Forwarded-For` is believed. |
| `RMCP_EDGE_BEHIND_PROXY` | Set when a reverse proxy fronts the edge. Empty trusted proxies then becomes a startup error. |
| `RMCP_EDGE_RATE_LIMIT_BURST` / `RMCP_EDGE_RATE_LIMIT_REFILL_PER_SEC` | Per-resolved-address edge budget, independent of any per-account limit. |

### Where the Anthropic range comes from

`RMCP_EDGE_ANTHROPIC_CIDRS` has **no default and no value written down in this
repository**, deliberately. Look it up at deploy time in **Anthropic's own
published IP-address documentation**, under the heading for the OUTBOUND /
egress ranges Claude uses when calling out to an external MCP server, and paste
that range in.

The range belongs to Anthropic and can change. A literal copied into our
`.env.example` or into the binary would go stale silently, and a stale allowlist
on this class does not fail loudly — it presents as a connector that simply
stopped being reachable, with nothing in our logs pointing at the cause. So the
lookup is a deploy step, and it is worth repeating if the connector ever goes
quiet.

Two traps on that page:

- Anthropic publishes an **inbound** range as well. That is where Anthropic
  *receives* connections; it is not what an inbound pinhole here wants. Use the
  **outbound** one.
- The outbound range is published for IPv4. Treat any IPv6 prefix as a
  deliberate decision you are making, not something to paste in by default.

### Fail-closed rules worth knowing before you debug a 403

- An **empty** CIDR list for a class denies that whole class. It is never read
  as "unrestricted".
- An **unparseable** policy, an unknown class name, a malformed CIDR,
  `RMCP_EDGE_BEHIND_PROXY` with no trusted proxies, or a rate-limit knob that is
  present but not a usable positive number is a **hard startup error**. The
  service will refuse to boot and say why in the journal. An ABSENT optional
  value is fine and takes its documented default — only a value you wrote and
  got wrong is fatal, because a security control that quietly reverts to a
  default is the failure nobody notices.
- A `.well-known`-style path that is not in the table is a `404` from the edge
  even from an allowed source.
- A path containing `%`, `.` or `..` segments is refused without being decoded.

## Getting the client address right

Everything the edge decides keys on one value: the resolved client address.

1. If the connection's peer is **not** in `RMCP_EDGE_TRUSTED_PROXIES`, the peer
   address is used and `X-Forwarded-For` is ignored **entirely**. From an
   untrusted peer that header is just attacker-supplied text.
2. If the peer **is** a trusted proxy, the **rightmost** entry in the chain that
   is not itself a trusted proxy is used. Never the leftmost — a client can
   prepend anything it likes to the chain, but it cannot remove the entry the
   proxy appended.

Two configuration mistakes break this, and both look fine in a single-machine
test:

- **A proxy that overwrites rather than appends.** `proxy_set_header
  X-Forwarded-For $remote_addr` replaces the chain. Use
  `$proxy_add_x_forwarded_for`.
- **A hop missing from `RMCP_EDGE_TRUSTED_PROXIES`.** Add a CDN or a second
  proxy in front and the edge will attribute every request to the hop it does
  not know about.

A trusted peer that yields **no untrusted address** is denied, not attributed to
itself. If your proxy is in `RMCP_EDGE_TRUSTED_PROXIES` but forwards no
`X-Forwarded-For` (or forwards a chain made only of other trusted proxies), every
request through it gets a 403 — because a proxy is not a client, and treating it
as one would admit everything it forwards whenever the proxy's own address
happens to sit in an allowed range. Fix the proxy config; do not "fix" it by
removing the hop from the trusted list.

The same applies when the entry that would have been chosen does not parse: the
request is denied rather than guessed at.

Paths are matched **exactly as received**. `/mcp/` is not `/mcp` — it is an
unlisted path and returns 404. Nothing legitimate sends the trailing form (the
canonical resource URI carries no trailing slash), and normalizing a path before
an authorization decision is a well-known bypass shape, so the edge does not.

## Bringing it up

1. **DNS.** Publish an A/AAAA record for the connector hostname pointing at the
   proxy's public address.
2. **TLS.** Issue a publicly-trusted certificate for that hostname (ACME at the
   proxy). The edge itself never terminates TLS.
3. **Proxy.** Start from `deploy/rmcp-edge-proxy.conf.example`. Proxy only the
   connector's own paths; return 404 for everything else.
4. **Firewall.** Forward 443 to the proxy host only, and mirror the per-path
   split at the network layer where your proxy can express it. The edge policy
   is defence in depth, not the only control.
5. **Service.** Deploy `deploy/rmcp-edge.service` (or copy its `RMCP_EDGE_*`
   block into your existing `terminus-primary` unit) and provide the values
   through an `EnvironmentFile`, never as literals in the unit.
6. **Verify from outside.** Both `.well-known` documents resolve; `/mcp`
   returns the `401` challenge; `/oauth/authorize` loads in a browser on one of
   your interactive networks; `/healthz` and `/admin/workers` return 404.

## Troubleshooting

| Symptom | Almost always |
|---|---|
| Discovery works, then consent shows a blank 403 | `/oauth/authorize` is being policed as `anthropic` — the single-pinhole mistake. Check `RMCP_EDGE_INTERACTIVE_CIDRS` covers the network your browser is actually on. |
| Everything 403s, from every network | A class with no CIDRs configured (an empty list denies), or every request is being attributed to the proxy because the proxy is not in `RMCP_EDGE_TRUSTED_PROXIES`. |
| The service will not start | An enabled edge with an unusable policy. The journal line names the variable. |
| A path 404s that you expected to work | It has no policy entry. Nothing outside the table above is served here. |
| 429s under normal use | The per-address edge budget. Note it counts refused requests too, deliberately. |
| Anthropic can reach `/mcp` from one datacentre but not another | The configured range does not cover Anthropic's full published outbound range. Use the outbound range, not the inbound one. |

Every decision — allowed, not exposed, source denied, rate limited — is emitted
as a structured record on the `rmcp_edge_audit` tracing target, with the
resolved client address, the path and the outcome. That log is the fastest way
to tell "the policy refused this" apart from "this never arrived".
