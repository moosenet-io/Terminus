//! Host-executor abstraction for the `agentsess_*` probes (AGSS-01).
//!
//! Every probe this module runs is a short, READ-ONLY command (`ps`, `tmux
//! list-panes`, `git rev-parse`). Two executors satisfy them:
//!
//! - [`LocalExecutor`] — the default. Runs on the box the terminus instance
//!   itself is on, which is what a per-host Terminus worker uses.
//! - [`DevSshExecutor`] — runs on the configured dev host by DELEGATING to
//!   `crate::dev`'s already-audited SSH session and quoting helpers. It opens
//!   no connection of its own and reads no credential of its own; if the dev
//!   door is not configured, this executor cannot be constructed at all.
//!
//! ## On subprocess use
//! The [`crate::tool::RustTool`] doc says tools should not shell out. That
//! guidance is aimed at tools whose real job is an HTTP/SQL call; a host
//! observability probe has no such equivalent, and the crate already runs
//! local commands this way in `crate::compiler`, `crate::intake`,
//! `crate::plane::prefix` and `crate::forge::git_transport`. What matters is
//! the safety property, which this module holds absolutely: **every command is
//! built in ARGV form and never passes through a shell**, so no caller-supplied
//! string can be interpreted as syntax. The SSH path is the one place a string
//! must reach a remote shell, and there it is single-quoted through `crate::dev`'s
//! existing escaper.

use async_trait::async_trait;

use crate::error::ToolError;

/// Result of one probe command.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// Runs read-only probe commands on some host.
#[async_trait]
pub trait HostExecutor: Send + Sync {
    /// Run `argv` (argv[0] is the program). Implementations MUST NOT pass this
    /// through a shell.
    async fn run(&self, argv: &[&str]) -> Result<CmdOutput, ToolError>;

    /// Label identifying the host in returned data (`local`, or the dev host label).
    fn host_label(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

/// Per-probe wall-clock cap. A probe is a sub-second command; anything that
/// blocks past this is a wedged host, and waiting longer would stall the whole
/// listing behind one bad probe.
const PROBE_TIMEOUT_SECS: u64 = 10;

pub struct LocalExecutor;

#[async_trait]
impl HostExecutor for LocalExecutor {
    async fn run(&self, argv: &[&str]) -> Result<CmdOutput, ToolError> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| ToolError::InvalidArgument("empty command".into()))?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(rest)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);

        let fut = cmd.output();
        let out = tokio::time::timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_SECS), fut)
            .await
            .map_err(|_| ToolError::Execution(format!("`{program}` timed out")))?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ToolError::NotFound(format!("`{program}` is not installed on this host"))
                } else {
                    ToolError::Execution(format!("failed to run `{program}`: {e}"))
                }
            })?;

        Ok(CmdOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn host_label(&self) -> &str {
        "local"
    }
}

// ---------------------------------------------------------------------------
// Dev host over the existing SSH door
// ---------------------------------------------------------------------------

pub struct DevSshExecutor {
    config: std::sync::Arc<crate::dev::DevConfig>,
    label: String,
}

impl DevSshExecutor {
    /// Build one from the dev module's env config. Returns `NotConfigured`
    /// when `DEV_HOST` is unset — deliberately NOT falling back to the local
    /// host, because silently reporting local sessions as the dev host's would
    /// misattribute whose work the caller is looking at.
    pub fn from_env() -> Result<Self, ToolError> {
        let config = crate::dev::DevConfig::from_env();
        let host = config.host.clone().ok_or_else(|| {
            ToolError::NotConfigured(
                "DEV_HOST is not set — the dev host cannot be observed until the dev SSH door is configured".into(),
            )
        })?;
        Ok(Self {
            config: std::sync::Arc::new(config),
            label: host,
        })
    }
}

#[async_trait]
impl HostExecutor for DevSshExecutor {
    async fn run(&self, argv: &[&str]) -> Result<CmdOutput, ToolError> {
        if argv.is_empty() {
            return Err(ToolError::InvalidArgument("empty command".into()));
        }
        // SSH inevitably hands a STRING to a remote shell, so every element is
        // single-quoted with `crate::dev`'s escaper — the same treatment the
        // dev tools already give user input.
        let quoted: Vec<String> = argv
            .iter()
            .map(|a| format!("'{}'", crate::dev::escape_single_quotes(a)))
            .collect();
        let command = quoted.join(" ");
        let res =
            crate::dev::run_ssh(std::sync::Arc::clone(&self.config), command, PROBE_TIMEOUT_SECS)
                .await?;
        Ok(CmdOutput {
            status: res.returncode,
            stdout: res.stdout,
            stderr: res.stderr,
        })
    }

    fn host_label(&self) -> &str {
        &self.label
    }
}

/// Resolve the executor for a caller-supplied `host` argument.
///
/// `None`/`"local"` → local; `"dev"` → the dev SSH door. Anything else is an
/// explicit error rather than a silent default, so a typo cannot quietly
/// return the wrong host's sessions.
pub fn executor_for(host: Option<&str>) -> Result<Box<dyn HostExecutor>, ToolError> {
    match host.unwrap_or("local") {
        "local" => Ok(Box::new(LocalExecutor)),
        "dev" => Ok(Box::new(DevSshExecutor::from_env()?)),
        other => Err(ToolError::InvalidArgument(format!(
            "unknown host '{other}' — expected 'local' or 'dev'"
        ))),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A `HostExecutor` that replays canned output keyed by argv[0], so every
    /// discovery/capture test runs with no processes, no tmux, and no network.
    pub struct FakeExecutor {
        responses: Mutex<HashMap<String, Result<CmdOutput, String>>>,
        /// (argv-substring, stdout) pairs, checked before the program map.
        argv_responses: Mutex<Vec<(String, String)>>,
        pub calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeExecutor {
        pub fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                argv_responses: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn with_stdout(self, program: &str, stdout: &str) -> Self {
            self.responses.lock().unwrap().insert(
                program.to_string(),
                Ok(CmdOutput {
                    status: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                }),
            );
            self
        }

        pub fn with_failure(self, program: &str, message: &str) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(program.to_string(), Err(message.to_string()));
            self
        }

        /// Respond differently depending on the FULL argv, not just the
        /// program — needed to give different pids different working
        /// directories, which is what makes an ordering/cap test able to
        /// reproduce a real starvation case rather than pass vacuously.
        pub fn with_stdout_matching(self, argv_contains: &str, stdout: &str) -> Self {
            self.argv_responses
                .lock()
                .unwrap()
                .push((argv_contains.to_string(), stdout.to_string()));
            self
        }

        /// A non-zero exit with a chosen stderr, for probes that FAIL rather
        /// than answer (a missing remote binary shows up this way, not as Err).
        pub fn with_exit(self, program: &str, status: i32, stderr: &str) -> Self {
            self.responses.lock().unwrap().insert(
                program.to_string(),
                Ok(CmdOutput {
                    status,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                }),
            );
            self
        }
    }

    #[async_trait]
    impl HostExecutor for FakeExecutor {
        async fn run(&self, argv: &[&str]) -> Result<CmdOutput, ToolError> {
            self.calls
                .lock()
                .unwrap()
                .push(argv.iter().map(|s| s.to_string()).collect());
            let joined = argv.join(" ");
            if let Some((_, out)) = self
                .argv_responses
                .lock()
                .unwrap()
                .iter()
                .find(|(needle, _)| joined.contains(needle.as_str()))
            {
                return Ok(CmdOutput {
                    status: 0,
                    stdout: out.clone(),
                    stderr: String::new(),
                });
            }
            match self.responses.lock().unwrap().get(argv[0]) {
                Some(Ok(o)) => Ok(o.clone()),
                Some(Err(m)) => Err(ToolError::Execution(m.clone())),
                None => Err(ToolError::NotFound(format!("`{}` not installed", argv[0]))),
            }
        }

        fn host_label(&self) -> &str {
            "test"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_for_rejects_unknown_host() {
        // `Box<dyn HostExecutor>` is not Debug, so `unwrap_err()` is unavailable
        // here — match on the error directly instead.
        match executor_for(Some("some-other-host")) {
            Err(ToolError::InvalidArgument(msg)) => {
                assert!(msg.contains("some-other-host"), "got {msg}")
            }
            Err(other) => panic!("wrong error variant: {other:?}"),
            Ok(_) => panic!("an unknown host must not silently resolve to an executor"),
        }
    }

    #[test]
    fn executor_for_defaults_to_local() {
        assert_eq!(executor_for(None).unwrap().host_label(), "local");
        assert_eq!(executor_for(Some("local")).unwrap().host_label(), "local");
    }

    #[tokio::test]
    async fn local_executor_reports_missing_program_as_not_found() {
        let err = LocalExecutor
            .run(&["definitely-not-a-real-program-agss01"])
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn local_executor_never_interprets_shell_syntax() {
        // Passing shell metacharacters as an ARGUMENT must not execute them:
        // `echo` receives them literally because there is no shell involved.
        // The canary lives in a unique temp dir, not a fixed `/tmp` path, so
        // concurrent runs of this test cannot observe each other's state.
        let dir = tempfile::tempdir().expect("temp dir");
        let canary = dir.path().join("shell-was-interpreted");
        let arg = format!("a; touch {}", canary.display());

        let out = LocalExecutor.run(&["echo", &arg]).await.unwrap();

        assert!(out.ok());
        assert!(out.stdout.contains("; touch"), "got {:?}", out.stdout);
        assert!(
            !canary.exists(),
            "the argument reached a shell and was executed — argv form must prevent this"
        );
    }
}
