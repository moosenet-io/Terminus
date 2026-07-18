## Redis backend — the shared fleet cache/queue/limiter (BLD-20)

terminus-primary owns the constellation's **one** Redis instance. It is a
single managed service with many consumers, addressed in-process through
`crate::redis` with **typed namespaces** so unrelated consumers never collide on
keys.

**Provisioning** (`deploy/redis/`): `install.sh` renders `redis.conf` +
`redis.service` from vault-materialized environment values and installs the
systemd unit. Redis binds **loopback + the mesh interface only**, requires a
password (`requirepass`, from the vault), and runs `appendonly` for durability.
`install.sh` is idempotent and **refuses to enable the service unless the
rendered config is loopback/mesh-only AND auth is set** — an authed ping must
succeed and an unauthenticated ping must be refused, both asserted post-install.
No endpoint, IP, port, or password is committed to any tracked file (S1/S7); the
endpoint reaches the process as `REDIS_URL` (password `REDIS_PASSWORD`),
materialized from the vault at boot.

**Namespaces** (`crate::redis::Namespace`) and their logical DB:

| Namespace | Key prefix | Logical DB | Durability |
|---|---|---|---|
| `Queue` | `queue:` | durable (DB 0) | never evicted (no TTL) |
| `Prefix` | `prefix:` | durable (DB 0) | never evicted (no TTL) |
| `Ratelimit` | `ratelimit:` | volatile (DB 1) | TTL'd, LRU-evictable |
| `Sccache` | `sccache:` | volatile (DB 1) | TTL'd, LRU-evictable |

Redis's eviction policy is **global**, not per-DB, so durable keys are protected
with `maxmemory-policy volatile-lru`: only keys carrying a TTL are eviction
candidates. Durable keyspaces (queue/overlay) are written with **no** TTL and
are therefore never evicted; volatile keyspaces carry a TTL and are the only
ones the cap can reclaim — the practical equivalent of "noeviction for the
durable DB, LRU for the volatile DB".

**Rate-limit + request-queue surface** (`crate::ratelimit`): `RedisRateLimiter`
implements the existing `gateway_framework::rate_limit::RateLimiter` trait — a
drop-in for the in-process limiter — using an **atomic Lua token-bucket** so N
concurrent over-limit requests are throttled correctly (no oversubscription) and
limits **survive a gateway restart** (the bucket lives in Redis, not process
memory). `RequestQueue` is a FIFO Redis-list queue wired into
`GatewayFramework::guard`'s admission path: when the limiter says over-limit, the
request is **admitted through the bounded FIFO queue** rather than 429'd
immediately — it waits (in FIFO order, up to `TERMINUS_GATEWAY_QUEUE_MAX_WAIT_MS`,
default 500ms) for a token to free, and only sheds load (429) when the queue is
full (`TERMINUS_GATEWAY_QUEUE_MAX_DEPTH`, default 128) or the wait times out. The
bounded enqueue is atomic (Lua `LLEN`-then-`RPUSH`), and an unreachable Redis
fails CLOSED (429). Every proxy request passes through `guard`, and the limiter
backend is chosen by **configuration**, not liveness: when `REDIS_URL` is set the
Redis limiter + queue are always selected (a configured-but-unreachable Redis
fails CLOSED at runtime, never a silent downgrade); only a genuinely absent
`REDIS_URL` uses the in-process limiter (with no queue).

All consumers share the ONE pooled `RedisBackend` — the prefix overlay included:
it stores its claims in the durable `prefix:overlay:v1` key via
`Namespace::Prefix` (durable DB), not a second connection or DB.

**Fail-safe degradation** (per consumer):
- The **rate-limiter fails CLOSED** for the proxy — an unreachable Redis denies,
  so an outage can never become an un-throttled flood at the backends.
- **sccache fails OPEN** to a local dir — a cache outage never blocks a build.
- The **prefix overlay fails OPEN** to the baseline TOML (`plane_prefix_*` still
  answer from the reviewed baseline when the overlay is unreachable). Wiring the
  overlay to this Redis makes `plane_prefix_register` **durable cross-instance**.
- The **queue** persists to Redis; a caller with an intake-DB fallback keeps
  working when Redis is down.

