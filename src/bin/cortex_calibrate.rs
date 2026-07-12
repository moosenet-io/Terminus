//! CXEG-10: calibration harness — retroactive false-positive tuning for the
//! CXEG-04 structural review (`cortex_review`) and CXEG-07 consistency lens
//! BEFORE either machinery is allowed to influence a live review.
//!
//! Replays the last N merged PRs of a project, scores each diff with
//! `cortex::review::compute_review` (CXEG-04) and `review::run_consistency_lens_dry`
//! (CXEG-07, dry/capture-only mode), and measures how often that scoring
//! WOULD have flagged code that in fact shipped and merged — a proxy false-
//! positive rate. Emits `docs/cortex-calibration.md` (or a caller-supplied
//! path) with the numbers, a per-signal breakdown, and a plain-language
//! threshold-tuning recommendation. The FP-rate math itself lives in
//! [`terminus_rs::cortex::calibrate`] as a pure, independently unit-tested
//! function; this binary is the (network-touching) driver around it.
//!
//! ## S9 — single door onto Gitea/GitHub
//! Every PR-list and diff-compare call in this file goes through
//! [`terminus_rs::forge`]'s provider-agnostic dispatch — the SAME mechanism
//! the `git_private`/`git_public` MCP tools use
//! (`ForgeRegistry::from_env().resolve(..)` then `ForgeProvider::dispatch`,
//! see `src/forge/git_private.rs`) — never a raw HTTP client built in this
//! file. There is no `reqwest`/`hyper`/etc. import here; `tests::no_direct_http_client`
//! below asserts that structurally by scanning this file's own source.
//!
//! ## Dry mode — no KGFIND writes, ever
//! This binary never calls `review::maybe_record_findings` (that function
//! isn't even exported outside `review::mod`) — it only calls
//! `review::run_consistency_lens_dry`, which is structurally incapable of
//! writing to the findings store (see that function's doc comment). Nothing
//! in this file imports or touches `FindingsStore`.
//!
//! ## Usage
//! ```text
//! cargo run --bin cortex_calibrate -- \
//!     --project-id TERM --owner moosenet --repo Terminus --n 50
//! ```
//! See `docs/cortex-calibration.md` for the report format and tuning
//! methodology, and `README.md`'s "Calibration" section for the full option
//! list and how to interpret the output.
//!
//! ## Known limitation (flagged honestly, not papered over)
//! The shared forge vocabulary's only diff-capable endpoint today is
//! `CommitsCompareDiff` (`GET /repos/{owner}/{repo}/compare/{basehead}`).
//! Depending on the Gitea/Forgejo server version, that endpoint's JSON body
//! may or may not carry a per-file `files` list (some versions expose only
//! commit metadata; per-file diffs are a separate `.diff`/`.patch`-suffixed
//! text endpoint outside today's `ForgeEndpoint` vocabulary). This harness
//! degrades cleanly rather than guessing: a PR whose compare response has no
//! recognizable file list is flagged `diff_unavailable: true`, counted in the
//! corpus total, but excluded from the SCORED sample (see
//! `cortex::calibrate::compute_fp_rate`) — never fabricated. If live runs
//! show most/all PRs landing in `diff_unavailable`, that's the honest signal
//! that a `PullRequestsListFiles`-shaped endpoint needs to be added to the
//! forge vocabulary in a follow-up item, not something to work around here.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde_json::{json, Value};

use terminus_rs::cortex::calibrate::{
    compute_fp_rate, looks_like_revert_or_hotfix, report_to_json, CalibrationKnobs, PrRecord,
    DEFAULT_MIN_SAMPLE, DEFAULT_TARGET_FP_RATE,
};
use terminus_rs::cortex::house_style::HouseStyleCache;
use terminus_rs::cortex::{CortexConfig, PROJECT_IDS};
use terminus_rs::forge::registry::{ForgePool, ForgeRegistry};
use terminus_rs::forge::{ForgeEndpoint, ForgeError, ForgeProvider, ForgeRequest};
use terminus_rs::review::{ProviderResult, ReviewConfig};

/// One page of `pull_requests_list` at a time; Gitea's own per-request cap is
/// enforced server-side too, this just keeps request sizes modest.
const PAGE_SIZE: u64 = 50;

#[derive(Parser, Debug)]
#[command(
    name = "cortex_calibrate",
    about = "CXEG-10: replay merged PRs through cortex_review + the CXEG-07 consistency lens \
             in dry mode, and report the would-have-flagged (false-positive proxy) rate."
)]
struct Args {
    /// Atlas KG project id the diffs are scored against (cortex_review /
    /// the consistency lens are keyed by this, not by the Gitea repo path).
    #[arg(long)]
    project_id: String,

    /// Gitea/Forgejo repo owner (org or user) the PR corpus is fetched from.
    #[arg(long)]
    owner: String,

    /// Gitea/Forgejo repo name the PR corpus is fetched from.
    #[arg(long)]
    repo: String,

    /// Explicit git-private forge provider id (e.g. "gitea", "forgejo").
    /// Default: the pool's configured default (see `ForgeRegistry::resolve`).
    #[arg(long)]
    provider: Option<String>,

    /// Target number of MERGED PRs to replay.
    #[arg(long, default_value_t = 50)]
    n: usize,

    /// Safety cap on how many list pages to fetch while looking for `n`
    /// merged PRs, so a repo with a huge closed-PR history (mostly unmerged/
    /// declined) can't turn this into an unbounded crawl.
    #[arg(long, default_value_t = 20)]
    max_pages: u64,

    /// Minimum SCORED sample size before the report is trusted enough to
    /// recommend a threshold change (below this, `sample_small: true`).
    #[arg(long, default_value_t = DEFAULT_MIN_SAMPLE)]
    min_sample: usize,

    /// Target false-positive rate (fraction, e.g. 0.10 = 10%).
    #[arg(long, default_value_t = DEFAULT_TARGET_FP_RATE)]
    target_fp_rate: f64,

    /// Include revert/hotfix-looking PRs in the scored sample instead of
    /// excluding them (they are always counted in the corpus total either way).
    #[arg(long, default_value_t = false)]
    include_reverts: bool,

    /// Force the CXEG-07 consistency lens on for this replay, even if
    /// `CORTEX_ENABLE_TIER_C` is unset/false in the environment. Calibration
    /// exists precisely to evaluate the lens before it's turned on live, so
    /// this defaults to true; pass `--consistency-lens false` to score
    /// structural-only (faster, no LLM calls).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    consistency_lens: bool,

    /// Where to write the generated report.
    #[arg(long, default_value = "docs/cortex-calibration.md")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    if !PROJECT_IDS.contains(&args.project_id.as_str()) {
        eprintln!(
            "cortex_calibrate: '--project-id {}' is not one of {:?}",
            args.project_id, PROJECT_IDS
        );
        return ExitCode::FAILURE;
    }

    match run(&args).await {
        Ok(report_md) => {
            if let Err(e) = std::fs::write(&args.out, &report_md) {
                eprintln!("cortex_calibrate: failed to write '{}': {e}", args.out.display());
                return ExitCode::FAILURE;
            }
            println!("cortex_calibrate: wrote {}", args.out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            // Fail cleanly: no partial report is EVER written on error (see
            // module doc's "Gitea tool unavailable ⇒ fail cleanly" guard) --
            // `run()` only returns `Ok` once the full corpus fetch + scoring
            // pass has completed, so there is no partial-write path here.
            eprintln!("cortex_calibrate: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &Args) -> Result<String, String> {
    let forge = ForgeRegistry::from_env();
    let provider = forge
        .resolve(ForgePool::Private, args.provider.as_deref())
        .map_err(|e| format!("git-private forge tool unavailable: {e}"))?;

    let mut records: Vec<PrRecord> = Vec::new();
    let mut merged_seen = 0usize;
    let mut page = 1u64;

    'paging: while page <= args.max_pages && merged_seen < args.n {
        let list_req = ForgeRequest::new(json!({
            "owner": args.owner,
            "repo": args.repo,
            "state": "closed",
            "limit": PAGE_SIZE,
            "page": page,
        }));
        let resp = provider
            .dispatch(ForgeEndpoint::PullRequestsList, list_req)
            .await
            .map_err(|e| forge_err_message("listing merged PRs", &e))?;

        let items = resp.body.as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            break 'paging;
        }

        for pr in &items {
            if merged_seen >= args.n {
                break 'paging;
            }
            let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
            if !merged {
                continue;
            }
            let number = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
            let title = pr.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let body = pr.get("body").and_then(Value::as_str);
            let is_revert_or_hotfix = looks_like_revert_or_hotfix(&title, body);

            let base_sha = pr.get("base").and_then(|b| b.get("sha")).and_then(Value::as_str);
            let head_sha = pr.get("head").and_then(|h| h.get("sha")).and_then(Value::as_str);

            let (changed_files, diff_unavailable) = match (base_sha, head_sha) {
                (Some(base), Some(head)) => fetch_changed_files(provider.as_ref(), args, base, head).await,
                _ => (Vec::new(), true),
            };

            if diff_unavailable {
                records.push(PrRecord {
                    number,
                    title,
                    merged,
                    is_revert_or_hotfix,
                    band: "unknown".to_string(),
                    structural_signals: Vec::new(),
                    consistency_categories: Vec::new(),
                    diff_unavailable: true,
                });
                merged_seen += 1;
                continue;
            }

            let (band, structural_signals) = score_structural(args, &changed_files).await;
            let consistency_categories = if args.consistency_lens {
                score_consistency(args, &changed_files).await
            } else {
                Vec::new()
            };

            records.push(PrRecord {
                number,
                title,
                merged,
                is_revert_or_hotfix,
                band,
                structural_signals,
                consistency_categories,
                diff_unavailable: false,
            });
            merged_seen += 1;
        }

        page += 1;
    }

    // Current knob values from the live config, so the report's recommendation
    // can emit a concrete `from → to` number, not just an env-var name.
    let cortex_config = CortexConfig::from_env();
    let knobs = CalibrationKnobs {
        tier_b_percentile: cortex_config.tier_b_percentile,
        dup_cosine: cortex_config.dup_cosine,
        risk_band_elevated_cut: cortex_config.risk_band_elevated_cut,
    };
    let report = compute_fp_rate(
        &records,
        !args.include_reverts,
        args.min_sample,
        args.target_fp_rate,
        &knobs,
    );
    Ok(render_markdown(args, &report, &records))
}

fn forge_err_message(action: &str, e: &ForgeError) -> String {
    format!("{action} failed: {e}")
}

/// Compactly format a knob value for the report table: integers without a
/// trailing `.0`, everything else to two decimals with trailing zeros
/// trimmed (so a percentile reads `93`, a cosine `0.95`).
fn trim_num(x: f64) -> String {
    if x.fract().abs() < 1e-9 {
        format!("{}", x as i64)
    } else {
        let s = format!("{x:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Resolve a PR's changed-file list via `CommitsCompareDiff` (S9: the same
/// forge dispatch path as everything else in this file). Returns
/// `(files, diff_unavailable)` -- `diff_unavailable` is true when the compare
/// response carries no recognizable per-file list (see module doc's "Known
/// limitation"), never when it carries an EMPTY list for a genuinely empty
/// diff (which cannot happen for a real merged PR, but is handled the same
/// safe way either way: no file list to score with).
async fn fetch_changed_files(
    provider: &dyn ForgeProvider,
    args: &Args,
    base_sha: &str,
    head_sha: &str,
) -> (Vec<String>, bool) {
    let req = ForgeRequest::new(json!({
        "owner": args.owner,
        "repo": args.repo,
        "basehead": format!("{base_sha}...{head_sha}"),
    }));
    let resp = match provider.dispatch(ForgeEndpoint::CommitsCompareDiff, req).await {
        Ok(r) => r,
        Err(_) => return (Vec::new(), true),
    };
    let files = extract_changed_files(&resp.body);
    let unavailable = files.is_empty();
    (files, unavailable)
}

/// Best-effort extraction of a per-file path list from a compare response,
/// tolerant of the shape variance called out in the module doc: tries a
/// top-level `files[].filename`/`files[].path` array first (the shape a
/// GitHub-style compare response uses), then falls back to any `filename`/
/// `path` fields nested under `commits[].files[]` (some Gitea/Forgejo
/// versions embed per-commit file stats there). Never fabricates a path.
fn extract_changed_files(body: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    // Takes `paths` as an explicit `&mut` param rather than capturing it, so it
    // holds no long-lived borrow that would conflict with the `paths.is_empty()`
    // read below (E0502).
    fn push_from_array(paths: &mut Vec<String>, arr: &[Value]) {
        for f in arr {
            if let Some(p) = f
                .get("filename")
                .or_else(|| f.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                paths.push(p);
            }
        }
    }
    if let Some(arr) = body.get("files").and_then(Value::as_array) {
        push_from_array(&mut paths, arr);
    }
    if paths.is_empty() {
        if let Some(commits) = body.get("commits").and_then(Value::as_array) {
            for c in commits {
                if let Some(arr) = c.get("files").and_then(Value::as_array) {
                    push_from_array(&mut paths, arr);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Score a PR's diff with CXEG-04's structural review. Degrades to
/// `("unknown", [])` if the project has no stored Atlas graph -- the same
/// degrade `cortex_review` itself returns (`configured: false`), never an
/// error (a missing graph for one project must not abort the whole replay).
async fn score_structural(args: &Args, changed_files: &[String]) -> (String, Vec<String>) {
    let config = CortexConfig::from_env();
    let response =
        terminus_rs::cortex::review::compute_review(&args.project_id, changed_files, &config, false).await;
    let band = response.get("band").and_then(Value::as_str).unwrap_or("unknown").to_string();
    let signals: Vec<String> = response
        .get("risk_signals")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("kind").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (band, signals)
}

/// Score a PR's diff with the CXEG-07 consistency lens in DRY/capture-only
/// mode (`review::run_consistency_lens_dry` — never writes to KGFIND; see
/// module doc). `enable_tier_c` is forced on for this call regardless of the
/// live environment's `CORTEX_ENABLE_TIER_C`, since calibration exists to
/// evaluate the lens BEFORE it's turned on live (see `--consistency-lens`'s
/// help text); every other `CortexConfig` field is read from the environment
/// as normal so the lens is scored against the SAME house-style thresholds a
/// live run would use.
async fn score_consistency(args: &Args, changed_files: &[String]) -> Vec<String> {
    let mut cortex_config = CortexConfig::from_env();
    cortex_config.enable_tier_c = true;

    let context = json!({
        "project_id": args.project_id,
        "changed_files": changed_files,
    });
    let review_cfg = ReviewConfig::from_env();
    let house_style_cache = HouseStyleCache::new();
    let panel_results: Vec<ProviderResult> = Vec::new();

    let run = terminus_rs::review::run_consistency_lens_dry(
        &context,
        "CXEG-10 calibration replay -- no live correctness panel",
        &panel_results,
        &review_cfg,
        &cortex_config,
        &house_style_cache,
    )
    .await;

    let mut categories: Vec<String> = run.findings.iter().map(|f| f.finding.category.clone()).collect();
    categories.sort();
    categories.dedup();
    categories
}

fn render_markdown(args: &Args, report: &terminus_rs::cortex::calibrate::CalibrationReport, records: &[PrRecord]) -> String {
    let mut out = String::new();
    out.push_str("# Cortex calibration report\n\n");
    out.push_str(&format!(
        "Generated by `cortex_calibrate` for project `{}` (`{}/{}`), replaying up to {} merged PR(s).\n\n",
        args.project_id, args.owner, args.repo, args.n
    ));
    out.push_str("## Summary\n\n");
    out.push_str(&format!("- Total PRs examined: **{}**\n", report.total_prs));
    out.push_str(&format!("- Scored (merged, diff available, not excluded): **{}**\n", report.scored_prs));
    out.push_str(&format!("- Excluded as revert/hotfix: **{}**\n", report.excluded_revert_hotfix));
    out.push_str(&format!("- Diff unavailable: **{}**\n", report.diff_unavailable));
    out.push_str(&format!("- Would have flagged: **{}**\n", report.would_have_flagged));
    out.push_str(&format!(
        "- False-positive rate: **{:.1}%** (target: {:.1}%)\n",
        report.false_positive_rate * 100.0,
        report.target_fp_rate * 100.0
    ));
    out.push_str(&format!("- Sample small (< {}): **{}**\n\n", report.min_sample, report.sample_small));

    out.push_str("## Per-signal firing rate\n\n");
    out.push_str("| signal | fired | sample | rate |\n|---|---:|---:|---:|\n");
    for s in &report.signal_rates {
        out.push_str(&format!("| {} | {} | {} | {:.1}% |\n", s.signal, s.fired, s.sample, s.rate * 100.0));
    }
    out.push('\n');

    out.push_str("## Recommendation\n\n");
    out.push_str(&report.recommendation);
    out.push_str("\n\n");
    if let Some(adj) = &report.recommended_adjustment {
        out.push_str("### Concrete threshold change\n\n");
        out.push_str("| env var | current | recommended |\n|---|---:|---:|\n");
        out.push_str(&format!(
            "| `{}` | {} | {} |\n\n",
            adj.env_var,
            trim_num(adj.current),
            trim_num(adj.recommended)
        ));
        out.push_str(&format!("{}\n\n", adj.rationale));
    }

    out.push_str("## Raw report (JSON)\n\n```json\n");
    out.push_str(&serde_json::to_string_pretty(&report_to_json(report)).unwrap_or_default());
    out.push_str("\n```\n\n");

    out.push_str(&format!("## Replayed PRs ({})\n\n", records.len()));
    out.push_str("| # | title | merged | band | revert/hotfix | diff unavailable |\n|---:|---|---|---|---|---|\n");
    for r in records {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.number,
            r.title.replace('|', "\\|"),
            r.merged,
            r.band,
            r.is_revert_or_hotfix,
            r.diff_unavailable
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_changed_files ────────────────────────────────────────────

    #[test]
    fn extracts_from_top_level_files_array_filename_key() {
        let body = json!({"files": [{"filename": "src/a.rs"}, {"filename": "src/b.rs"}]});
        assert_eq!(extract_changed_files(&body), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn extracts_from_top_level_files_array_path_key() {
        let body = json!({"files": [{"path": "src/a.rs"}]});
        assert_eq!(extract_changed_files(&body), vec!["src/a.rs"]);
    }

    #[test]
    fn falls_back_to_per_commit_files_when_no_top_level_files() {
        let body = json!({
            "commits": [
                {"sha": "abc", "files": [{"filename": "src/a.rs"}]},
                {"sha": "def", "files": [{"filename": "src/b.rs"}]}
            ]
        });
        assert_eq!(extract_changed_files(&body), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn returns_empty_when_no_recognizable_file_list() {
        // e.g. a Gitea/Forgejo version whose compare response is commit
        // metadata only -- must degrade to empty (caller then flags
        // diff_unavailable), never fabricate a path.
        let body = json!({"commits": [{"sha": "abc", "message": "x"}], "total_commits": 1});
        assert!(extract_changed_files(&body).is_empty());
    }

    #[test]
    fn dedups_and_sorts_paths() {
        let body = json!({"files": [{"filename": "src/b.rs"}, {"filename": "src/a.rs"}, {"filename": "src/b.rs"}]});
        assert_eq!(extract_changed_files(&body), vec!["src/a.rs", "src/b.rs"]);
    }

    // ── S9: no direct HTTP client in this binary ────────────────────────

    #[test]
    fn no_direct_http_client() {
        // Structural check that this file never imports/uses reqwest (or any
        // other HTTP client) directly -- every network call must go through
        // the forge dispatch path (`ForgeProvider::dispatch`), never a raw
        // client built in this binary (S9). Scans this file's OWN source so
        // the assertion survives future edits without relying on a reviewer
        // remembering to check by hand.
        let src = include_str!("cortex_calibrate.rs");
        // Skip this doc-comment/test's own mentions of "reqwest" by only
        // scanning code outside of `///`/`//!` doc comments and this test's
        // own string literal — simplest robust approach: check for the
        // actual import/usage tokens rather than the bare substring "reqwest",
        // which only ever appears in this file inside comments/doc text.
        assert!(!src.contains("use reqwest"), "must not import reqwest directly");
        assert!(!src.contains("reqwest::Client"), "must not construct a reqwest client directly");
        assert!(!src.contains("hyper::Client"), "must not construct a hyper client directly");
    }

    // ── revert/hotfix detection re-export sanity (calibrate module owns the
    // exhaustive test coverage; this just confirms the binary wires the same
    // function, not a local reimplementation) ──────────────────────────────

    #[test]
    fn revert_detection_is_reused_from_the_calibrate_module() {
        assert!(looks_like_revert_or_hotfix("Revert \"feat: x\"", None));
        assert!(!looks_like_revert_or_hotfix("feat: add cortex_calibrate", None));
    }
}
