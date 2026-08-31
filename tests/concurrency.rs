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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_creates_in_a_new_project_get_distinct_numbers() {
    let w = Arc::new(World::fresh("td_concurrent").await);
    const FRESH: &str = "acme/brand-new";

    let one = tokio::spawn({
        let w = Arc::clone(&w);
        async move { w.try_create(FRESH, U1, "one").await }
    });
    let two = tokio::spawn({
        let w = Arc::clone(&w);
        async move { w.try_create(FRESH, U1, "two").await }
    });

    let one = one
        .await
        .expect("join")
        .expect("the first create must succeed");
    let two = two
        .await
        .expect("join")
        .expect("the second create must succeed too");

    let mut numbers = [one.number, two.number];
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        [1, 2],
        "two concurrent creates in an empty project were handed the same number"
    );
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
