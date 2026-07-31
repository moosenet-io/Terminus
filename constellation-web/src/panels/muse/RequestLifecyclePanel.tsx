// MGUI-08 (S129): muse.requests.detail — one request's lifecycle. Guide screen 06,
// "Request lifecycle — search → decide → grab".
//
// A PARAMETERIZED detail route (`/muse/requests/:id`), registered `hideInRail` like
// `muse.library.detail`: it is reached from a request row, never from navigation, and
// a rail/palette entry pointing at `:id` would have no id to navigate to.
//
// WHAT THE ENDPOINT ACTUALLY GIVES US. `GET /api/requests/:id` (Muse's
// `web/dashboard.rs::get_request_detail`) returns `{found, request, status, steps,
// terminal}` — the `media_requests` row plus the fixed happy-path stepper
// (`requested → approved → searching → grabbed → available`) with each stop marked
// `reached | current | pending` from the row's real status. Every stepper state this
// panel draws is Muse's, computed server-side; none is re-derived here, so the panel
// cannot disagree with the backend about where a request stands.
//
// `GET /api/requests` returns `{requests: [], tiers: {}, total: 0}` on this deployment,
// so there is no id to sample against and every screenshot of this panel today is a
// not-found. That is a statement about what the LIST endpoint returned — it is not
// evidence that the request path, the wanted worker, or anything else did or did not
// run, and this panel must not say otherwise.
//
// ── THE BIG OMISSION: the guide's `decide_release · winner` card ─────────────
//
// The guide's third block shows the winning release with "format score +240",
// "quality tier 1080p WEB-DL ✓ cutoff", and "seeders 184 · freeleech". NONE of those
// four values has a backing field on any Muse read endpoint, so per house rule 1 the
// card is OMITTED rather than rendered with placeholders:
//
//   - `decide_release` (Muse `src/decision/mod.rs`) is a PURE, in-process function.
//     Its `ReleaseChoice { total_score, quality_tier, reason }` is consumed by
//     `acquisition::fulfill_request` and then dropped — it is not written to
//     `media_requests` (whose columns are id/provider_ids/media_kind/title/
//     requested_by/status/tier/quality_profile_id/note/monitored_item_id/timestamps)
//     nor to `download_queue`, and no endpoint returns it.
//   - seeders/freeleech come from an `Availability` computed ON DEMAND at
//     `POST /requests` time (`http/requests.rs`) to classify the tier. It is likewise
//     never persisted. `/api/indexers/rss` does carry a `seeders` figure, but for
//     recent releases across all indexers — attaching one of those to a request would
//     be inventing a decision that was never made.
//
// Surfacing that scoring properly needs Muse to persist the decision, which is a
// backend change (Muse must write the `ReleaseChoice` alongside the queue row) and so
// sits outside MGUI-08, whose scope is this read-only GUI panel. Tracked as MUSE #104
// — filed after this panel was written, which is why the branch itself declined to
// cite a number rather than invent one. Until that lands,
// the panel states the absence in one line and shows what IS real instead: the
// download-queue row for this request (release title, indexer, protocol, size), which
// is the release that was genuinely grabbed.
//
// ── The safety gates are DISPLAY-ONLY ────────────────────────────────────────
// Identical reasoning to `SettingsPanel`'s acquisition section, reused deliberately so
// the two surfaces cannot drift into disagreeing about what "safe" means. See
// `gateResult` below.
import { useParams } from 'react-router-dom';
import { ChartCard } from '../../viz/ChartCard';
import {
  useMuseRequestDetail,
  useMuseDownloadQueue,
  useMuseAcquisitionGate,
  type MuseRequestStep,
  type MuseDownloadQueueRow,
} from '../../hooks/useMuse';

/** States a section that exists in the design but whose value is absent. States the
 *  OBSERVED absence only — never a diagnosis of why (same contract as
 *  `MediaDetailPanel`'s `AbsenceNote`, which was corrected twice for exactly that). */
function AbsenceNote({ what }: { what: string }) {
  return (
    <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', fontStyle: 'italic', lineHeight: 1.5 }}>
      {what}
    </div>
  );
}

function Label({ children }: { children: React.ReactNode }) {
  return (
    <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
      {children}
    </div>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div style={{ display: 'flex', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)', padding: '3px 0' }}>
      <span style={{ color: 'var(--text-400, var(--text-300))', minWidth: 120, fontFamily: 'var(--font-mono)' }}>{label}</span>
      <span style={{ color: 'var(--text-100)', wordBreak: 'break-word' }}>{value}</span>
    </div>
  );
}

// ── Lifecycle stepper ────────────────────────────────────────────────────────

const STEP_COLOR: Record<string, string> = {
  reached: 'var(--ok, #4ade80)',
  current: 'var(--accent, #8b5cf6)',
  pending: 'var(--text-400, var(--text-300))',
};

function Step({ step, last }: { step: MuseRequestStep; last: boolean }) {
  // An unrecognized state falls through to the neutral token rather than being
  // coerced to `pending` — a state we don't understand must not be reported as one
  // we do. Same rule the subsystem grid applies to its wiring vocabulary.
  const color = STEP_COLOR[step.state] ?? 'var(--text-300)';
  const filled = step.state === 'reached' || step.state === 'current';
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', flex: last ? '0 0 auto' : 1, minWidth: 0 }}>
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 3, flex: '0 0 auto' }}>
        <div
          style={{
            width: 10,
            height: 10,
            borderRadius: '50%',
            background: filled ? color : 'transparent',
            border: `1px solid ${color}`,
          }}
        />
        <span style={{ fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color, whiteSpace: 'nowrap' }}>
          {step.label}
        </span>
        {/* The state word is shown, not just encoded in a colour — colour alone is
            not an accessible carrier for the one fact this row exists to convey. */}
        <span style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', whiteSpace: 'nowrap' }}>
          {step.state}
        </span>
      </div>
      {!last && (
        <div aria-hidden style={{ height: 1, flex: 1, background: step.state === 'reached' ? color : 'var(--border)', marginBottom: 22 }} />
      )}
    </div>
  );
}

// ── Safety gates ─────────────────────────────────────────────────────────────

/** The dual-gate verdict, stated ONLY when it is actually determinable.
 *
 *  guide GATE 1 · master : `ExperienceSettings.acquisition.enabled`  (in /api/settings)
 *  guide GATE 2 · tier   : `MUSE_ARR_REQUEST_AUTO_TIER_ENABLED`      (env; NOT exposed)
 *
 *  Muse's own gate is `acquisition_enabled && arr_request_auto_tier_enabled &&
 *  download_client_configured` (`http/requests.rs`), and `acquisition_enabled` is
 *  itself `master_enabled && acquisition.enabled`. So:
 *
 *    gate 1 off (either boolean) ⇒ SAFE, soundly, whatever gate 2 is: the request is
 *                                  persisted for review and never actioned.
 *    gate 1 on                   ⇒ INDETERMINATE from here. Armed requires gate 2 as
 *                                  well, and this surface cannot see gate 2. On a
 *                                  live-grab switch a wrong guess is dangerous in
 *                                  BOTH directions, so neither is made.
 *
 *  Returns `null` for "cannot be determined", never a default. */
export function gateResult(gate1: boolean | null): 'safe' | null {
  return gate1 === false ? 'safe' : null;
}

function StatePill({ on, onLabel = 'on', offLabel = 'off' }: { on: boolean; onLabel?: string; offLabel?: string }) {
  return (
    <span
      style={{
        padding: '1px 8px',
        fontSize: 'var(--fs-2xs, 10px)',
        fontFamily: 'var(--font-mono)',
        textTransform: 'uppercase',
        letterSpacing: '0.04em',
        borderRadius: 'var(--radius-xs, 3px)',
        color: on ? 'var(--ok, #4ade80)' : 'var(--text-400, var(--text-300))',
        border: `1px solid ${on ? 'var(--ok, #4ade80)' : 'var(--border)'}`,
        whiteSpace: 'nowrap',
      }}
    >
      {on ? onLabel : offLabel}
    </span>
  );
}

/** Distinct from off: a value this surface genuinely cannot see. Rendering it as
 *  `off` would be an invented fact, and on a safety gate that is the dangerous
 *  direction. */
function UnknownPill() {
  return (
    <span
      style={{
        padding: '1px 8px',
        fontSize: 'var(--fs-2xs, 10px)',
        fontFamily: 'var(--font-mono)',
        textTransform: 'uppercase',
        letterSpacing: '0.04em',
        borderRadius: 'var(--radius-xs, 3px)',
        color: 'var(--text-400, var(--text-300))',
        border: '1px dashed var(--border)',
        whiteSpace: 'nowrap',
      }}
    >
      unknown
    </span>
  );
}

function GateRow({ label, detail, right }: { label: string; detail: string; right: React.ReactNode }) {
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: '1fr auto',
        gap: 'var(--space-2)',
        alignItems: 'center',
        padding: '5px 0',
        borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))',
      }}
    >
      <div style={{ minWidth: 0 }}>
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }}>{label}</div>
        <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', fontFamily: 'var(--font-mono)' }}>{detail}</div>
      </div>
      <div>{right}</div>
    </div>
  );
}

function SafetyGates() {
  const { data, loading, degraded } = useMuseAcquisitionGate();
  // `?? null` is UNKNOWN, not off: a still-loading or degraded settings read must not
  // read as a definite negative on a safety gate.
  const gate1 = data ? data.master_enabled && data.acquisition.enabled : null;
  const result = gateResult(gate1);

  return (
    <ChartCard title="Safety gates · both required" subtitle="write-path · display only" height={220} loading={loading} degraded={degraded}>
      <div style={{ height: '100%', overflowY: 'auto' }}>
        <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', marginBottom: 'var(--space-2)', lineHeight: 1.5 }}>
          Shown as <strong>state, not controls</strong>. Both must be on before a live grab can
          fire; either one off means a request is persisted for review and never actioned. The
          guide renders these as toggles — they are read-only here because this is a browse
          surface and flipping them has real-world blast radius.
        </div>
        <GateRow
          label="Gate 1 · master"
          detail="ExperienceSettings.acquisition.enabled (AND master_enabled)"
          right={gate1 === null ? <UnknownPill /> : <StatePill on={gate1} />}
        />
        <GateRow
          label="Gate 2 · tier"
          detail="MUSE_ARR_REQUEST_AUTO_TIER_ENABLED — not exposed by /api/settings"
          right={<UnknownPill />}
        />
        <GateRow
          label="Result"
          detail={
            result
              ? 'gate 1 is off, so a request is persisted for review and never actioned — this holds whatever gate 2 is'
              : 'cannot be determined here: gate 1 is not off, and gate 2 is not visible to this surface'
          }
          right={result ? <StatePill on={false} offLabel="safe" /> : <UnknownPill />}
        />
      </div>
    </ChartCard>
  );
}

// ── Grab ─────────────────────────────────────────────────────────────────────

function formatSize(bytes: number | null): string | null {
  if (bytes === null || bytes === undefined) return null;
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${v >= 10 || u === 0 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

/** What the guide's "decide_release · winner" card becomes when you may only render
 *  fields that exist: the download-queue row(s) whose `request_id` is this request —
 *  the release that was actually grabbed for it. Title/indexer/protocol/size are real
 *  persisted columns. The scoring is not; see the module doc. */
function GrabbedRelease({ requestId }: { requestId: number | null }) {
  const { data, loading, degraded } = useMuseDownloadQueue();
  // Only rows for THIS request. A row with a null `request_id` came from the wanted
  // worker via `monitored_item_id` and belongs to no request — matching `null == null`
  // would attach some other title's grab to this request.
  const rows: MuseDownloadQueueRow[] =
    requestId === null ? [] : (data?.queue ?? []).filter(q => q.request_id === requestId);
  const empty = !loading && !degraded && rows.length === 0;

  return (
    <ChartCard
      title="Grab"
      subtitle="download_queue · persisted columns only"
      height={220}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No grab recorded for this request"
      emptyHint="No download_queue row references this request id"
    >
      <div style={{ height: '100%', overflowY: 'auto' }}>
        {rows.map(q => (
          <div key={q.id} style={{ padding: '5px 0', borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))' }}>
            <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', wordBreak: 'break-all' }}>{q.release_title}</div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', fontFamily: 'var(--font-mono)' }}>
              {/* Only the fields the row actually carries — an absent indexer/protocol/
                  size contributes nothing rather than an "unknown" placeholder. */}
              {[q.status, q.indexer, q.protocol, formatSize(q.size_bytes)].filter(Boolean).join(' · ')}
            </div>
            {/* No progress bar. Muse hard-codes `progress: null` (documented SEAM:
                per-torrent progress is not persisted), and a 0% bar would assert this
                download has made no progress — a different claim from "we don't know
                how far along it is". */}
          </div>
        ))}
        <div style={{ marginTop: 'var(--space-2)' }}>
          <AbsenceNote what="No decide_release scoring is shown: format score, cutoff comparison, seeders and freeleech are computed in-process at decision time and are not persisted on the request or the queue row, so no read endpoint returns them." />
        </div>
      </div>
    </ChartCard>
  );
}

// ── Panel ────────────────────────────────────────────────────────────────────

export function RequestLifecyclePanel() {
  const { id } = useParams<{ id: string }>();
  const { data, loading, degraded } = useMuseRequestDetail(id ?? null);

  // `found: false` carries neither `request` nor `steps` — both are optional and must
  // never be dereferenced before this check.
  const notFound = !loading && !degraded && data !== null && data.found === false;
  const request = data?.request ?? null;
  const steps = data?.steps ?? [];
  const terminal = data?.terminal ?? null;

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <ChartCard
        title={loading ? 'Loading…' : (request?.title ?? 'Request')}
        subtitle={[request?.media_kind, request?.requested_by, request?.created_at].filter(Boolean).join(' · ')}
        height={260}
        loading={loading}
        degraded={degraded}
        empty={notFound}
        emptyMessage="Request not found"
        emptyHint={`No media_request with id ${id ?? '—'}`}
      >
        <div style={{ height: '100%', overflowY: 'auto' }}>
          <Label>Lifecycle</Label>
          {steps.length > 0 ? (
            <div style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--space-2)', padding: '4px 0 10px' }}>
              {steps.map((s, i) => (
                <Step key={s.label} step={s} last={i === steps.length - 1} />
              ))}
            </div>
          ) : (
            <AbsenceNote what="No lifecycle steps in the response." />
          )}

          {/* Denied/failed are terminal and OFF the happy path — Muse surfaces them in
              `terminal` rather than inventing an intermediate step, so the stepper
              alone would not show them. */}
          {terminal && (
            <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--warn, #fbbf24)', fontFamily: 'var(--font-mono)', padding: '2px 0 6px' }}>
              terminal · {terminal}
            </div>
          )}

          <Label>Request</Label>
          {/* `status` is the row's raw text status, echoed verbatim. */}
          {data?.status && <Row label="status" value={data.status} />}
          {/* A null tier means the row was never classified. Rendered as an explicit
              absence rather than "unclassified"/"NeedsReview" — a tier is a safety
              classification and defaulting one would be inventing a decision. */}
          {request &&
            (request.tier !== null && request.tier !== undefined ? (
              <Row label="tier" value={request.tier} />
            ) : (
              <AbsenceNote what="No tier recorded on this request." />
            ))}
          {request?.quality_profile_id !== null && request?.quality_profile_id !== undefined && (
            <Row label="quality profile" value={String(request.quality_profile_id)} />
          )}
          {request?.monitored_item_id !== null && request?.monitored_item_id !== undefined && (
            <Row label="monitored item" value={String(request.monitored_item_id)} />
          )}
          {request?.note && <Row label="note" value={request.note} />}
          {request?.updated_at && <Row label="updated" value={request.updated_at} />}
        </div>
      </ChartCard>

      <SafetyGates />
      <GrabbedRelease requestId={request?.id ?? null} />
    </div>
  );
}
