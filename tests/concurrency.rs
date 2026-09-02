//! The per-project number allocator, and what the engine says when it cannot
//! hand out a number right now.

mod support;

use std::sync::Arc;

use support::{World, U1};

/// `SELECT MAX(number) + 1 ... FOR UPDATE` takes NO LOCK when the result set is
/// empty — there is no row to lock. So in a project with no tasks yet, two
/// concurrent creates both read 1, both try to insert 1, and the UNIQUE turns
/// one of them into a duplicate-key failure or a deadlock.
///
/// A new project is the case that matters: it is every project's first minute.
///
/// **THE BARRIER AND THE ROUNDS ARE THE TEST.** Two bare `tokio::spawn`s were
/// what this asserted before, and that shape does not reliably overlap: MEASURED
/// against a mutant with the `FOR UPDATE` deleted from the allocator's read, it
/// passed 14 runs out of 15. A regression test that survives the deletion of the
/// lock it exists to pin is a test that has stopped guarding anything. The
/// `Barrier` releases both allocators into the read-then-update window together,
/// and the rounds mean one lucky interleaving cannot carry the whole assertion —
/// the same shape ADR-0513 records for `iam-db`'s claim race, and for the same
/// reason.
///
/// A FRESH PROJECT PER ROUND, because the empty table is the case under test.
/// Reusing one project would make every round after the first an allocation
/// against a counter row that already exists, which is the case that was never
/// broken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_creates_in_a_new_project_get_distinct_numbers() {
    let w = Arc::new(World::fresh("td_concurrent").await);

    for round in 0..12 {
        let fresh = format!("acme/brand-new-{round}");
        let gate = Arc::new(tokio::sync::Barrier::new(2));

        let racers: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|title| {
                let w = Arc::clone(&w);
                let gate = Arc::clone(&gate);
                let fresh = fresh.clone();
                tokio::spawn(async move {
                    gate.wait().await;
                    w.try_create(&fresh, U1, title).await
                })
            })
            .collect();

        let mut numbers = Vec::new();
        for racer in racers {
            numbers.push(
                racer
                    .await
                    .expect("join")
                    .unwrap_or_else(|e| panic!("round {round}: a create must succeed: {e}"))
                    .number,
            );
        }

        numbers.sort_unstable();
        assert_eq!(
            numbers,
            [1, 2],
            "round {round}: two concurrent creates in an empty project were handed the same number"
        );
    }
}

/// Serialising the allocator makes contention REAL, so the error it produces
/// under contention has to carry the right code. A lock wait timeout means
/// "try again"; `INTERNAL` means "do not". Flattening the one retryable engine
/// error into a non-retryable status is how a transient wait becomes a
/// user-visible failure.
#[tokio::test]
async fn a_lock_wait_is_aborted_and_therefore_retryable() {
    let w = World::impatient("td_lockwait").await;
    const P: &str = "acme/contended";

    // The counter row has to exist before it can be held.
    w.create(P, U1, "seed").await;

    let mut holder = w.pool.acquire().await.expect("acquire");
    sqlx::query("START TRANSACTION")
        .execute(&mut *holder)
        .await
        .expect("begin");
    sqlx::query("SELECT next_number FROM task_counter WHERE project_id = ? FOR UPDATE")
        .bind(P)
        .fetch_one(&mut *holder)
        .await
        .expect("hold the counter");

    let err = w
        .try_create(P, U1, "blocked")
        .await
        .expect_err("the allocator cannot proceed while the counter is held");
    assert_eq!(
        err.code(),
        tonic::Code::Aborted,
        "a lock wait timeout is retryable and must not be reported as INTERNAL"
    );

    sqlx::query("ROLLBACK")
        .execute(&mut *holder)
        .await
        .expect("release");
}
