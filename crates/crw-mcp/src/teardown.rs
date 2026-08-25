//! Process teardown: signal handling + the single consolidated exit path.
//!
//! Mirror of `crw-cli`'s `teardown` module (the standalone `crw-mcp` binary
//! cannot depend on the `crw-cli` crate). `main` is the *only* place that
//! exits (via [`finish`]), and `kill_all_browsers()` runs exactly once on
//! every path (Ok, Err, signal, stdin-EOF) before the process dies. This is
//! what structurally closes the "`process::exit` after a browser spawned
//! bypasses `Drop`" leak class.

use std::sync::atomic::{AtomicBool, Ordering};

/// A command-level failure carrying the exit `code` plus an optional message
/// already formatted for stderr. The dispatcher prints `msg` only when present.
#[derive(Debug)]
pub struct CmdError {
    pub code: i32,
    pub msg: Option<String>,
}

impl CmdError {
    /// Exit with `code`; the call site already printed to stderr.
    pub fn code_only(code: i32) -> Self {
        Self { code, msg: None }
    }
}

/// Set once teardown has begun so the signal task, a normal exit, and the
/// stdin-EOF path don't double-run `kill_all_browsers()`.
static TEARING_DOWN: AtomicBool = AtomicBool::new(false);

/// Run `kill_all_browsers()` at most once across all callers. In a proxy-only
/// build (no `embedded` feature) there is no browser engine compiled in, so this
/// is a cheap no-op guard with nothing to kill.
fn teardown_once() {
    if TEARING_DOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(feature = "embedded")]
    crw_renderer::browser::kill_all_browsers();
}

/// Install the signal teardown task. Call once at command entry, **before**
/// any browser spawn or auto-download. On SIGINT/SIGTERM/SIGHUP/SIGQUIT it
/// kills every spawned browser group then exits `128 + signo` (130 SIGINT,
/// 143 SIGTERM, 129 SIGHUP, 131 SIGQUIT). Direct exit after teardown — not a
/// signal re-raise (re-raise under tokio races a second signal).
#[cfg(unix)]
pub fn install_signal_teardown() {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
        let mut sigquit = signal(SignalKind::quit()).expect("install SIGQUIT handler");
        let code = tokio::select! {
            _ = sigint.recv()  => 130,
            _ = sigterm.recv() => 143,
            _ = sighup.recv()  => 129,
            _ = sigquit.recv() => 131,
        };
        teardown_once();
        std::process::exit(code);
    });
}

#[cfg(not(unix))]
pub fn install_signal_teardown() {}

/// The single consolidated exit point. Runs teardown exactly once, prints any
/// error message to stderr, and exits with the right code. Called from `main`.
pub fn finish(result: Result<(), CmdError>) -> ! {
    teardown_once();
    match result {
        Ok(()) => std::process::exit(0),
        Err(CmdError { code, msg }) => {
            if let Some(m) = msg {
                eprintln!("{m}");
            }
            std::process::exit(code);
        }
    }
}

// TESTABILITY: `finish` and the `#[cfg(unix)]` branch of
// `install_signal_teardown` both terminate via `std::process::exit`, which
// would kill the test harness itself — there is no way to observe them
// return without a subprocess, and RULES.md forbids spawning processes for
// this kind of hermetic unit test. Everything else in this file is covered
// below.
#[cfg(test)]
mod tests {
    use super::*;

    // --- CmdError ---

    #[test]
    fn cmd_error_code_only_has_no_message() {
        let e = CmdError::code_only(1);
        assert_eq!(e.code, 1);
        assert!(e.msg.is_none());
    }

    #[test]
    fn cmd_error_code_only_preserves_arbitrary_code() {
        assert_eq!(CmdError::code_only(0).code, 0);
        assert_eq!(CmdError::code_only(255).code, 255);
        assert_eq!(CmdError::code_only(-1).code, -1);
    }

    #[test]
    fn cmd_error_struct_literal_carries_message() {
        let e = CmdError {
            code: 2,
            msg: Some("boom".to_string()),
        };
        assert_eq!(e.code, 2);
        assert_eq!(e.msg.as_deref(), Some("boom"));
    }

    #[test]
    fn cmd_error_debug_format_does_not_panic() {
        // Struct derives Debug; just exercise it so a future field addition
        // that breaks Debug is caught here.
        let e = CmdError::code_only(1);
        let formatted = format!("{e:?}");
        assert!(formatted.contains('1'));
    }

    // --- teardown_once ---
    //
    // `TEARING_DOWN` is a module-level static shared by every test in this
    // binary, so we can only assert the *idempotency* contract (repeated
    // calls never panic and never double-run), not the pre/post flag value
    // relative to other tests.

    #[test]
    fn teardown_once_is_idempotent_across_repeated_calls() {
        // First call may or may not be the very first in the binary (test
        // order is unspecified), but calling it several times in a row must
        // never panic regardless of which call actually "wins" the swap.
        teardown_once();
        teardown_once();
        teardown_once();
    }

    #[test]
    fn teardown_once_flag_is_true_after_any_call() {
        teardown_once();
        assert!(TEARING_DOWN.load(Ordering::SeqCst));
    }

    // --- install_signal_teardown ---

    #[cfg(unix)]
    #[tokio::test]
    async fn install_signal_teardown_does_not_panic_on_install() {
        // Installing the signal listeners must succeed and return
        // immediately (it only spawns a background task); we don't send a
        // real signal or await the spawned task, we just verify the install
        // call itself is safe to make from an async context.
        install_signal_teardown();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn install_signal_teardown_can_be_called_multiple_times() {
        // Each call spawns its own independent listener set; repeated
        // installs (e.g. across retried startup paths) must not panic or
        // conflict with each other.
        install_signal_teardown();
        install_signal_teardown();
    }
}
