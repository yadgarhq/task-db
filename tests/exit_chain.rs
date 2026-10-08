//! THE EXIT CHAIN, held at the binary (ledger 748): the two ways a running
//! `task-db` is asked to stop both end in a drain and exit code 0.
//!
//! COPIED FROM `yadgarhq/project-db`'s own `tests/exit_chain.rs` (its C-DB1
//! unit, #54), not from `yadgarhq/task`'s: `project-db` is the sibling `-db`
//! twin whose `rotate::watch_set` is the SAME shape this repository's is
//! (listener TLS, the database credential, `database.sslCaSecret`'s
//! authority, and the mounted schedule document — see `src/rotate.rs`), and
//! its harness is the one already measured green in CI against both cases
//! below.
//!
//! The library tests prove the parts — `yadgar-lifecycle` that a signal
//! resolves `shutdown()` and that a changed file resolves `rotate::watch`,
//! `tests/assembly.rs` that the watch set holds the right files. None of
//! them can see `main`'s `stop_when` wiring those futures into the `select!`
//! that stops the server. Deleting either arm compiled and passed every
//! suite here. Only running the binary can tell.
//!
//! **What each case kills, measured by mutating `src/main.rs`:**
//!
//! - Delete the `signals` binding AND its `select!` arm: no handler is ever
//!   installed, SIGTERM takes the kernel default, the status carries signal
//!   15 and no code, and [`sigterm_drains_and_exits_zero`]'s `Some(0)` is
//!   red.
//! - Delete the arm ONLY: the handler is installed when `boot::shutdown()` is
//!   CALLED and tokio never removes it, so SIGTERM is swallowed and the
//!   server keeps serving. The exit wait's deadline fires; the test kills
//!   the child and fails.
//! - Delete the rotation arm: nothing polls the watcher, so
//!   [`a_rewritten_shared_document_drains_and_exits_zero`] never sees the
//!   CHANGED line and fails on its deadline.
//!
//! A failing drain is NOT one of these cases. `Drain::Overran` exits 0 by
//! design and `Drain::Finished(Err)` exits 1 either way, so nothing about
//! the drain's own outcome moves the code this file asserts.
//!
//! **A REAL ENGINE IS NEEDED**, unlike `task`'s own copy of this file: D7's
//! capability probe and the migration both run before `main` ever listens,
//! so `wait_until_listening` only returns once both have succeeded against
//! [`support::dsn`]'s engine — the same one every other integration test in
//! this crate already migrates against. There is no engine-less shortcut:
//! `boot::pool_config` refuses to construct a pool without one.
//!
//! See `tests/support/mod.rs` for why each run is in its own mount namespace.

mod support;

use std::os::unix::process::ExitStatusExt;

use support::{describe, fresh_boot_database, Booted, FIXTURE, MARGIN, POLL, SPLAY_MAX};
use yadgar_lifecycle::DRAIN_BUDGET;

/// A fresh database and the environment pointing the binary at it, one pair
/// per test — `tokio::test` runs each on its own thread of one process, so a
/// shared name would race two cases against one schema (same argument
/// `World::fresh` makes).
async fn cleartext_env(unique: &str, root: &std::path::Path) -> Vec<(String, String)> {
    let db_name = format!("yadgar_task_db_exit_chain_{unique}");
    fresh_boot_database(&db_name).await;
    support::boot_cleartext_env(&db_name, &root.join("db-password"))
}

/// Case (i): kubelet's SIGTERM ends the process with 0.
///
/// The deadline is the drain's whole budget plus a margin, not less: an
/// `Overran` drain is a legal exit 0 that arrives one budget after the
/// signal.
#[tokio::test]
async fn sigterm_drains_and_exits_zero() {
    let root = tempfile_dir("sigterm");
    let env = cleartext_env("sigterm", &root).await;
    let mut task_db = Booted::start(&env);
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

/// Case (ii): a rewritten watched file ends the process with 0.
///
/// `shared.yaml` is the one watched file a cleartext deployment has besides
/// the engine credential (ADR-0523): the rewrite keeps the schedule valid and
/// changes the bytes, which is all the watcher compares.
#[tokio::test]
async fn a_rewritten_shared_document_drains_and_exits_zero() {
    let root = tempfile_dir("rotate");
    let env = cleartext_env("rotate", &root).await;
    let mut task_db = Booted::start(&env);
    task_db.wait_until_listening();

    task_db.rewrite_shared(&format!("{FIXTURE}# rotated by tests/exit_chain.rs\n"));
    // One poll to notice, the splay, then the drain's whole budget.
    let deadline = POLL + SPLAY_MAX + DRAIN_BUDGET + MARGIN;
    let started = std::time::Instant::now();

    let changed = task_db.wait_for_line("the watcher's CHANGED line", deadline, |l| {
        l.contains("have CHANGED on disk")
    });
    assert!(
        changed.contains("shared.yaml"),
        "the CHANGED line must name the file that changed: {changed}"
    );
    // TLS is off and `database.sslCaSecret` is unset, so both the serving
    // and the engine-CA fingerprints are reported as `none`; this module
    // dials nothing itself, so there is no `client` fingerprint pair at all
    // (unlike `task`, which reaches `task-db` and so carries one).
    for field in ["\"serving_before\":\"none\"", "\"serving_after\":\"none\""] {
        assert!(
            changed.contains(field),
            "the CHANGED line must carry {field}: {changed}"
        );
    }

    let status = task_db.wait_for_exit(
        "after the watched file changed",
        deadline.saturating_sub(started.elapsed()),
    );
    assert_eq!(
        status.code(),
        Some(0),
        "a rotation is not an error; it must drain and exit 0 ({})",
        describe(Some(status))
    );
}

/// A per-test scratch directory for the password file `cleartext_env` writes
/// into. Separate from `Booted`'s own `fresh_root` (the mount namespace's
/// upper/work/shared), because the password file must stay visible from
/// OUTSIDE the overlay that namespace puts over `/etc` — this directory is
/// never under `/etc` and the overlay never touches it.
fn tempfile_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "yadgar-task-db-exit-chain-fixture-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("the fixture directory must be created");
    dir
}
