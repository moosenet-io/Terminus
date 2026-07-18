## Fleet clock — `time_now` (CLK-01)

`time_now` is a core tool that returns the **authoritative fleet date/time**
straight from the system clock, so agents and the `review_run` capstone gate
time-based decisions (e.g. enforcing the Fable-OAuth window through
2026-07-19) on the real clock rather than a drift-prone harness date. It is a
pure system-clock read — no network, no secrets, no hardcoded values.

Output (JSON):

| Field | Meaning |
| --- | --- |
| `utc_iso8601` | Current instant as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`, UTC). |
| `unix` | Unix epoch seconds. |
| `date` | `YYYY-MM-DD` in the requested zone (default UTC). |
| `time` | `HH:MM:SS` in the requested zone (default UTC). |
| `weekday` | Full weekday name in the requested zone. |
| `tz` | The zone the `date`/`time`/`weekday` fields are rendered in (`"UTC"` by default). |
| `note` | Present only when an invalid `tz` was supplied — explains the UTC fallback. |

Optional argument `tz` (IANA name, e.g. `"America/New_York"`) renders the
local `date`/`time`/`weekday`; `utc_iso8601` and `unix` always describe the
same UTC instant regardless of `tz`. An **invalid** `tz` is not an error — the
tool falls back to UTC and adds a `note` field naming the bad zone.

```json
{ "utc_iso8601": "2026-07-12T20:30:45Z", "unix": 1783888245,
  "date": "2026-07-12", "time": "20:30:45", "weekday": "Sunday", "tz": "UTC" }
```

