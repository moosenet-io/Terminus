# CONST-01 — harmony-web Inventory + Constellation Adaptation Plan

**Spec:** S97-constellation-gui (v3.22) · **Item:** CONST-01 (discovery/documentation) ·
**Audience:** <operator> + the executing agents for CONST-02..14. **This is the authoritative stack reference
— CONST-02..14 cite this rather than re-inspecting or guessing.**

---

## 1. Stack inventory (base app = `harmony/harmony-web`)

The Constellation UI is adapted from the existing Harmony web app. Verified from
`harmony/harmony-web/package.json`, `vite.config.ts`, `tsconfig.json`, and `src/**`:

| Concern | Choice | Version | Notes |
|---|---|---|---|
| Framework | React | 18.3.1 | Function components + hooks throughout |
| Language | TypeScript | 5.4.5 | `tsc --noEmit` is the typecheck gate |
| Build/dev | Vite | 5.3.1 | `dev` / `build` (=`tsc --noEmit && vite build`) / `preview` |
| Styling | Tailwind CSS | 3.4.4 | + PostCSS 8 / autoprefixer 10; CSS vars (`--h-bg`, `--h-teal`, `--h-border`) |
| Charts | Recharts | 3.8.1 | Analytics pages (`components/analytics/*`) |
| Routing | react-router-dom | 6.23.1 | `BrowserRouter basename="/"`, `<Routes>`/`<Route>` |
| State | React local state + custom hooks | — | No Redux/Zustand; per-domain hooks under `src/hooks/` |
| Live data | native WebSocket | — | `hooks/useWebSocket.ts`, event types in `types/events.ts` |
| Backend calls | native `fetch` | — | `hooks/useApi.ts`; **same-origin** via `apiUrl()` |

**How it calls the backend today (the key adaptation surface):**
- `src/hooks/useApi.ts` — `apiUrl(path) = ${window.location.origin}${path}` (NO hardcoded host — good),
  `getAuthHeaders()` reads an API key from `localStorage['harmony_soma_api_key']`, and a `useApi<T>()`
  hook wrapping `fetch` with 401 handling.
- `src/hooks/useAuth.ts` — session-cookie auth: `GET /api/auth/me` on mount, `POST /api/auth/logout`;
  `credentials:'include'`. This is the primary auth; the API key is a CLI-caller fallback.
- `src/App.tsx` — the shell: gates on `useAuth` (`<Login>` when unauthenticated), `Sidebar` + `StatusStrip`
  + `ConversationBar`, a `<Routes>` table of 12 pages, and `useWebSocket` driving live state.
- Domain hooks (`useChord`, `useChordAnalytics`, `useChordHealth`, `useProviders`, `useRoutingState`,
  `useExecutorState`, `useRalphState`, `useTreeData`, `useEscalationData`) each `fetch` a Harmony/Chord
  endpoint at the same origin. **These are the direct-backend calls that must be routed through the
  aggregation client.**

**Page/route inventory (`src/pages/`):** Dashboard, Projects, Agents, Inference, Providers, Tasks, PRs,
Analytics, Prompts, Sessions, Playground, AuditLog, Login. **Provenance note:** components carry `SGUI-02`
markers — an earlier "Soma GUI" spec built this app against Harmony's Soma API.

---

## 2. Reuse map

**Reuse directly (the generic shell + primitives):**
- Shell chrome: `App.tsx` structure, `components/Sidebar.tsx`, `components/StatusStrip.tsx`,
  `components/ConversationBar.tsx`, `components/Card.tsx`, `MetricCard`, `ProgressBar`, `Skeleton`.
- Auth: `hooks/useAuth.ts` + `pages/Login.tsx` + `components/LoginModal.tsx` — the session-cookie model is
  exactly CONST-03's model; generalize label ("Harmony" → "Constellation") and point at the aggregation
  auth routes.
- Data plumbing: `hooks/useApi.ts` (the `apiUrl` + `useApi<T>` pattern) and `hooks/useWebSocket.ts` — keep
  the shape, swap the transport target to the aggregation client.
- Analytics: `components/analytics/*` + Recharts usage — reusable for CONST-05/06/12 read views.
- Inference widgets: `components/inference/*` (VRAMGauge, ModelInventory, ProviderHealthCard,
  LifecycleControls, StorageManager) — high-value reuse for the CONST-05 Chord panel.
- Tailwind theme + `styles/globals.css` / `styles/interactions.css` — keep the design system.

**Generalize (Harmony-specific → multi-system):**
- `App.tsx` routing table + `Sidebar` nav — currently a flat Harmony menu; becomes system-grouped nav
  (Harmony / Chord / Lumina / Terminus / Providers / Status) driven by the **module registry**.
- The domain hooks that `fetch` Harmony/Chord endpoints directly — re-point through the aggregation client
  namespaces (`/api/harmony/*`, `/api/chord/*`, …).
- Copy/branding: "Harmony — Soma Dashboard", "Harmony Soma connected." → Constellation.

**Replace (does not meet the control-plane bar):**
- `hooks/useApi.ts` `localStorage['harmony_soma_api_key']` secret store — **this is a
  secret-in-browser-storage smell and violates the spec's "no browser storage of secrets" rule.** Replace
  with in-memory/session state + the aggregation session cookie (CONST-03/CONST-04).
- The 401 `prompt('Enter Harmony API key')` flow — replace with the login page + session.

---

## 3. Adaptation plan (harmony-web → Constellation)

**Target location:** a new `constellation-web/` app directory in `moosenet/Terminus`, seeded from
`harmony/harmony-web`, so the UI + the aggregation layer that serves it live in one repo/binary (matches
"Terminus hosts the UI"). Terminus serves the built `dist/` assets and the `/api/*` aggregation routes at
the same origin — preserving the same-origin model harmony-web already assumes (no CORS, cookie auth works).

**Three concrete adaptations:**
1. **Extract a generic shell.** Keep `App.tsx`'s auth-gate + `Sidebar`/`StatusStrip` layout; replace the
   hardcoded page table with a registry-driven nav + route set (§4). Rename Harmony branding.
2. **Introduce the aggregation client** (`constellation-web/src/lib/aggregationClient.ts`): the single
   typed entry point for `/api/{harmony,chord,lumina,terminus}/*`. Every existing domain hook's `fetch`
   is re-pointed through it. Ships with a **mock adapter** so the front-end builds and is testable before
   CONST-02 exists (same TS interface, swapped by env — no code change to go live).
3. **Introduce the module registry** (`constellation-web/src/lib/moduleRegistry.ts`): panels register a
   descriptor `{ id, system, title, available, component }`; the shell renders only `available` panels.
   Availability comes from the aggregation layer's registry endpoint (CONST-02) — a not-yet-built
   capability (S94/S95/S96 absent) simply isn't registered and never renders.

**Auth adaptation (CONST-03):** reuse `useAuth`'s session-cookie model verbatim against the aggregation
auth routes (`/api/auth/{me,login,logout}` on Terminus); drop the API-key/`localStorage` path entirely;
hold no backend creds client-side.

---

## 4. Gap list (what the control plane needs that harmony-web lacks)

1. **Multi-system nav + registry-driven rendering** — harmony-web has a flat, hardcoded Harmony menu; the
   control plane needs system-grouped nav that appears/disappears with registered capabilities (CONST-04).
2. **The aggregation client abstraction** — today hooks call the backend directly; the control plane needs
   the single-client indirection so the browser only ever talks to Terminus (CONST-02/04).
3. **Per-system CONFIG panels** — harmony-web is largely read/monitor + a few controls; the control plane
   needs active config forms per system (CONST-05..08) and per provider (CONST-09..11).
4. **Vault-reference secret UI** — no concept today; needs a control that shows a vault KEY NAME + a
   set/rotate affordance that never round-trips a value to the browser (CONST-08, cross-cutting).
5. **Tiered confirmation gating** — needs a reusable confirmation primitive for impactful/destructive
   mutations (provider creds, Ansible playbook allowlist hard-confirm, PII-gate/`mirror_ready` toggles)
   (CONST-07/09/13).
6. **In-memory-only session auth** — remove the `localStorage` API-key store (CONST-03/04).
7. **Backend-availability degradation UX** — a panel/route must render "unavailable" cleanly when its
   backend is down or absent, not error (CONST-04 registry + CONST-12 status).

---

## 4b. Page → panel reuse map (LEVERAGE THIS — operator directive: reuse harmony-web maximally)

harmony-web is ~7,500 LOC (13 pages, 42 components, 13 hooks). Most Constellation panels are a
**generalization of an existing harmony-web page**, not a from-scratch build. The downstream panel items
(CONST-05/06/12) should START from the mapped page + components and only re-point the backend calls
through the aggregation client (namespaced) + register under the module registry.

| Constellation panel (item) | System | Reuse directly from harmony-web |
|---|---|---|
| CONST-05 Chord (inference/routing/serving) | Chord | `pages/Inference.tsx` + `components/inference/*` (VRAMGauge, ModelInventory, ModelDownload, LifecycleControls, StorageManager, ProviderHealthCard); `pages/Providers.tsx` + ProviderCard/ProviderAnalytics/ProviderSummary + RoutingDiagram + InferenceMixSlider; hooks `useChord`, `useChordHealth`, `useProviders`, `useRoutingState` |
| CONST-06 Harmony (build pipeline) | Harmony | `pages/{Projects,Tasks,Agents,PRs,Prompts,Sessions}.tsx` + BuildControls/EngineControls/TaskTree/AgentLane/EscalationStepper/HeldTasksPanel; hooks `useExecutorState`, `useRalphState`, `useTreeData`, `useEscalationData` |
| CONST-12 Fleet status (read-only) | Status | `pages/{Dashboard,Analytics}.tsx` + `components/dashboard/*` (EngineNode/EnginePanel/FlowConnection/WorkerNode) + `components/analytics/*` (CostChart/ProviderPerformance/SavingsHero/TokenUsageChart) + StatusStrip/MetricCard; `useChordAnalytics` |
| CONST-13 secret-masking/audit | (cross-cut) | `pages/AuditLog.tsx` as the audit-view surface |
| CONST-07 Lumina / CONST-08 Terminus / CONST-09..11 providers | Lumina/Terminus/providers | Mostly NEW config forms, but reuse the shell primitives (Card, forms, confirmation) + the vault-ref control pattern |

Net: CONST-05, CONST-06, and CONST-12 are **substantially pre-built** by harmony-web — the work is
re-pointing hooks through the aggregation client + registry wiring, not building UI. Only Lumina/Terminus/
provider config forms are genuinely new. This is the reuse the operator directed; CONST-04's port pulls
the whole app across so these items start from working pages.

## 5. Build sequencing implication (for the orchestrator)

- **UI-side items build now, no Terminus compile:** CONST-04 (shell + typed aggregation client w/ mock
  adapter + module registry) and every panel front-end (CONST-05..12) can be scaffolded against the mock
  adapter in a worktree immediately.
- **WAIT on TMOD for the Terminus-side aggregation layer (CONST-02/03/08 backend):** the modular-broker
  refactor decides whether the aggregation layer is a compiled-in core module or a federated worker. Wire
  the live adapter once that settles; until then the mock adapter keeps the UI green.
- **Provider panels degrade until their backend lands:** CONST-10 (MEDIA/S94 — actively building),
  CONST-11 (DOC/S95 — shipped), CONST-09 (GIT/S96) register only when their domain reports available.
