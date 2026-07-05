//! HFIX-07: operator/tooling CLI over `intake::gpu_authority` — inspect and
//! change the GPU's operating mode by hand, outside of an automated sweep.
//!
//! ## Usage
//! - `gpu_mode status` — print the current lock (if any) and whether the
//!   exclusive-mode Ollama drop-in is present. No side effects.
//! - `gpu_mode acquire <exclusive|shared> <holder>` — apply that mode's
//!   policy and record `holder` as the lock owner. Fails if a DIFFERENT,
//!   still-alive holder already has it.
//! - `gpu_mode release <holder>` — release `holder`'s lock, restarting
//!   whatever competing services THIS acquire had stopped. Does not revert
//!   Ollama's runner config — `gpu_mode acquire shared <holder>` for that.
//!
//! Run as root (same trust level as `intake_coder_sweep` and
//! `intake::lifecycle`, which also shell out to `systemctl` directly).

use terminus_rs::intake::gpu_authority::{self, GpuMode};

fn parse_mode(s: &str) -> Result<GpuMode, String> {
    match s.trim().to_lowercase().as_str() {
        "exclusive" => Ok(GpuMode::Exclusive),
        "shared" => Ok(GpuMode::Shared),
        other => Err(format!("unknown mode '{other}' — expected 'exclusive' or 'shared'")),
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  \
         gpu_mode status\n  \
         gpu_mode acquire <exclusive|shared> <holder>\n  \
         gpu_mode release <holder>"
    );
}

fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("status") => {
            let st = gpu_authority::status();
            match st.lock {
                Some((holder, mode, pid, alive)) => println!(
                    "lock: holder={holder} mode={mode} pid={pid} pid_alive={alive}"
                ),
                None => println!("lock: none"),
            }
            println!("ollama_exclusive_dropin_present: {}", st.ollama_dropin_present);
            std::process::ExitCode::SUCCESS
        }
        Some("acquire") => {
            let (Some(mode_s), Some(holder)) = (args.get(1), args.get(2)) else {
                print_usage();
                return std::process::ExitCode::FAILURE;
            };
            let mode = match parse_mode(mode_s) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("{e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match gpu_authority::acquire(mode, holder) {
                Ok(()) => {
                    println!("acquired mode={} holder={holder}", mode.as_str());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("acquire failed: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Some("release") => {
            let Some(holder) = args.get(1) else {
                print_usage();
                return std::process::ExitCode::FAILURE;
            };
            match gpu_authority::release(holder) {
                Ok(()) => {
                    println!("released holder={holder}");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("release failed: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        _ => {
            print_usage();
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_accepts_known_values_case_insensitively() {
        assert_eq!(parse_mode("exclusive").unwrap(), GpuMode::Exclusive);
        assert_eq!(parse_mode("EXCLUSIVE").unwrap(), GpuMode::Exclusive);
        assert_eq!(parse_mode(" shared ").unwrap(), GpuMode::Shared);
    }

    #[test]
    fn parse_mode_rejects_unknown() {
        assert!(parse_mode("turbo").is_err());
    }
}
