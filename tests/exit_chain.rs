//! THE EXIT CHAIN, held at the binary (ledger 748): SIGTERM ends a running
//! `task-db` with a drain and exit code 0.
//!
//! COPIED FROM `yadgarhq/task`'s own `tests/exit_chain.rs` at origin/main
//! (task#70), per the C-DB1 card. `tests/shutdown.rs` already proves that
//! `boot::shutdown` and `boot::server` drain on SIGTERM IN-PROCESS — calling
//! the library functions directly, with no binary spawned. That is NOT the
//! same claim as this file's: `main() -> ExitCode` could still wire those
//! two functions together wrongly, or `init_tracing`/`configure` could
//! panic before either is reached, and no in-process test can see it.
//! `boot_message.rs`'s own header states the same distinction for refusals;
//! this is its counterpart for the drain.
//!
//! **What this case kills, measured by mutating `src/main.rs`:** delete the
//! `signals` binding and its `select!` arm in `stop_when`, and no handler is
//! ever installed — SIGTERM takes the kernel default, the status carries
//! signal 15 and no code, and this test's `Some(0)` assertion goes red.
//!
//! **NOT PORTED FROM `task`'s COPY:** the rotation-triggered case (a
//! rewritten `shared.yaml` draining the server on its own). This binary's
//! watch set carries the database credential and `database.sslCaSecret`'s
//! authority besides the shared document (`rotate::watch_set`), and
//! reproducing that shape safely — three material kinds, one of them a real
//! engine credential — is filed as follow-up rather than guessed at here.
//! `tests/support/mod.rs`'s own header names the same gap.
//!
//! **A REAL ENGINE IS NEEDED**, unlike `task`'s copy of this file: this
//! binary probes and migrates before it ever listens (D7, D69), so
//! `Booted::start` cannot reach "listening" without one. `YADGAR_TEST_DSN`
//! must name a user that can `CREATE`/`DROP DATABASE`.

mod support;

use std::os::unix::process::ExitStatusExt;

use support::{describe, Booted, MARGIN};
use yadgar_lifecycle::DRAIN_BUDGET;

/// kubelet's SIGTERM ends the process with 0.
///
/// The deadline is the drain's whole budget plus a margin, not less: an
/// `Overran` drain is a legal exit 0 that arrives one budget after the
/// signal.
#[test]
fn sigterm_drains_and_exits_zero() {
    let mut task_db = Booted::start(&[]);
    task_db.wait_until_listening();

    task_db.terminate();
    let status = task_db.wait_for_exit("after SIGTERM", DRAIN_BUDGET + MARGIN);

    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must drain and exit 0 ({}); signal 15 here means no handler was \
         installed",
        describe(Some(status))
    );
    assert_eq!(status.signal(), None);
    assert!(
        task_db
            .seen()
            .iter()
            .any(|l| l.contains("draining in-flight requests") && l.contains("SIGTERM")),
        "the drain must name the signal that started it"
    );
}
