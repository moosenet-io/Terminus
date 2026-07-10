# Networking

Terminus's authenticated surface — the plain `/enroll` endpoint and the mTLS
tool-dispatch listener — normally binds to loopback or a private interface
only (see [`docs/deploy/client.md`](../deploy/client.md) and
[`docs/deploy/personal-services.md`](../deploy/personal-services.md) for the
exact defaults). To reach it from outside the primary's own host, you need
some network path that gets your client's traffic to that interface in the
first place.

This directory covers the two supported options:

| | [WireGuard](wireguard.md) | [Tailscale](tailscale.md) |
|---|---|---|
| Control plane | None — you own both ends | Hosted coordination server (Tailscale's, or a self-hosted [Headscale](https://github.com/juanfont/headscale)) |
| Setup effort | Manual keypairs, manual peer config, manual NAT/firewall traversal | `tailscale up` on each device; NAT traversal handled for you |
| Addressing | Static tunnel IPs you assign | Stable per-device IP + MagicDNS name, independent of the underlying network |
| Access control | Firewall rules on the primary's tunnel interface | Tailscale ACLs (tags, groups) enforced at the coordination-server level |
| Best for | A small, fixed set of clients; no third-party dependency; full control over the tunnel | A client fleet that grows/changes, multiple networks/NATs, or when you want centrally auditable ACLs |
| Underlying protocol | WireGuard | WireGuard (Tailscale is a managed WireGuard mesh) |

Both are just **transport** — they get a client's TCP/UDP traffic to the
primary gateway host. Neither one authenticates or authorizes a client's use
of Terminus's tools. That's still the job of the mTLS enrollment flow
described in [`docs/deploy/client.md`](../deploy/client.md): a per-identity
shared secret exchanged for a short-lived client certificate + JWT, verified
on every connection by the primary's mTLS listener
([`src/pki/mtls.rs`](../../src/pki/mtls.rs)). Treat the network layer and the
identity layer as two independent gates that both have to pass — a compromised
tunnel peer still can't call a tool without a valid enrollment, and a leaked
enrollment secret still can't reach the listener without network access to
it.

## Which one should I pick?

- **Pick WireGuard** if you're connecting a handful of your own machines and
  you'd rather not depend on any hosted coordination service — you generate
  the keys, you write the peer configs, you control the whole tunnel.
- **Pick Tailscale** if you want zero-config NAT traversal, a stable
  MagicDNS name instead of tracking IPs, and you're comfortable with (or
  already run) a coordination plane — Tailscale's own, or a self-hosted
  Headscale instance if you don't want to depend on Tailscale's servers
  either.
- **Already on the same LAN, or reaching the primary through some other VPN
  you already run?** You don't need either of these — just point
  `TERMINUS_PRIMARY_URL` / `TERMINUS_MTLS_HOST` at whatever address already
  reaches the primary, and skip straight to
  [`docs/deploy/client.md`](../deploy/client.md).

## Firewall posture, either way

The primary gateway host should firewall its plain enroll port and its mTLS
port so they're reachable **only** from the tunnel/tailnet interface, not the
public internet — see each page's "Firewall notes" section for the concrete
rules. Binding those listeners to `127.0.0.1` and letting the tunnel software
itself be the only path in (a reverse proxy on the tunnel interface, or the
tunnel's own routing) is the simplest way to guarantee that.

---

Back to the [documentation index](../README.md).
