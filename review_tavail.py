import sys, json; sys.path.insert(0,'<path>/.claude/jobs/6fec76b4/tmp')
from mcp import MCP
m=MCP(timeout=1800)
diff=open('<path>/.claude/jobs/6fec76b4/tmp/tavail.diff').read()
criteria = """- [ ] Registry carries an availability state with reason + last_verified
- [ ] Agent-facing tools/list excludes non-available tools; admin view shows all WITH state
- [ ] tools/call fails closed on a non-available tool
- [ ] Availability composes with the identity filter rather than replacing it
- [ ] Malformed config fails closed to `off`
- [ ] Unconfigured default preserves today's behaviour exactly
- [ ] No hardcoded infrastructure values in new/modified code
- [ ] All existing tests still pass"""
ctx = {
 "item_title":"TAVAIL-01: tool availability state — registry-visible, agent-unavailable (Terminus)",
 "diff":diff,
 "approach":"Operator requirement: dead tools must NOT be de-registered — they stay VISIBLE in the registry but sit in an OFF position, unavailable to agents. Motivating evidence: crucible_*/odyssey_* point at a decommissioned host; soma_* is retired (superseded by the terminus-hosted webgui); dashboard_*'s FastAPI gateway is gone. Design: (a) src/availability.rs — Availability{Available,Off,Broken} + AvailabilityPolicy parsed from TERMINUS_TOOL_AVAILABILITY_JSON, exact-name > longest-prefix, FAIL CLOSED (an unrecognised state resolves to Off, never Available, so an operator typo cannot silently re-expose a dead tool); absent config => everything Available (byte-for-byte pre-change behaviour). (b) tools/list filter applied AFTER the MESH-08 filter_catalog_for_principal so authorization and availability COMPOSE — availability can only remove, never re-grant. (c) tools/call fail-closed gate BEFORE dispatch so a stale cached catalog cannot invoke a parked tool; the denial names the state+reason rather than 'not found' (a 'not found' is what sent the model hunting in the deep_research bug). (d) src/availability_tool.rs — tool_availability, the ADMIN view listing every registered tool with state+reason, which is what keeps 'visible in the registry' true for the human.",
 "edge_cases":"A tool parked while an agent holds a cached catalog -> the call-time gate is what actually protects. Availability must NOT be auto-inferred from one failed probe (a flaky upstream like the <host> mesh would flap tools off) — automatic proposal, operator confirmation. 'off' must be distinguishable from 'not authorized for you' in the admin view so a scoped guest surface is not misread as breakage. A OnceLock policy means changing availability needs a service restart — deliberate, matches every other Environment= knob on this service.",
 "readme_note":"This adds a new operator-facing tool (tool_availability) and a new env var (TERMINUS_TOOL_AVAILABILITY_JSON) — README update is arguably REQUIRED here. I did NOT update it. Please flag this if you agree it is a feature-adding change.",
 "project_id":"TERM",
}
r = m.call("review_run", {"structure":"panel_unanimous",
   "providers":["codex","free","opus"], "criteria":criteria, "context":ctx})
open('<path>/.claude/jobs/6fec76b4/tmp/review_tavail.json','w').write(r)
try:
    d=json.loads(r)
    print('AGGREGATE:', d['aggregate_verdict'], '| complete:', d['complete'])
    for p in d['providers']:
        print('---', p['provider'], '=>', p['verdict'])
        print((p.get('reasoning') or '')[:1600])
        for f in (p.get('findings') or []):
            print('  FINDING[%s/%s] %s: %s' % (f.get('severity'),f.get('category'),f.get('symbol'),(f.get('description') or '')[:350]))
except Exception as e:
    print(r[:2500])
