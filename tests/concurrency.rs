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
/// what this asserted before, and that shape does not reliably overlap: measured
/// against a mutant of the allocator as it then was, it passed 14 runs out of
/// 15. A regression test that survives the deletion of what it exists to pin is
/// a test that has stopped guarding anything. The `Barrier` releases both
/// allocators into the same allocation against the same absent key, and the
/// rounds mean one lucky interleaving cannot carry the whole assertion — the
/// same shape ADR-0513 records for `iam-db`'s claim race, and for the same
/// reason.
///
/// **The mutant named above no longer exists**, and saying otherwise would make
/// this comment describe code the repository does not have. The allocator does
/// not read and then update: it is one `INSERT ... ON DUPLICATE KEY UPDATE`
/// followed by a read-back, with no `FOR UPDATE` anywhere. The mutation that
/// kills this test today is dropping the `+ 1` from that statement — MEASURED,
/// and it takes this test and its neighbour red on round zero.
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

/// **AS MANY CONCURRENT CREATES AS THE POOL HAS CONNECTIONS, AND ALL OF THEM
/// MUST SUCCEED.** What this asserts is a RATIO rather than a count, which is
/// the whole reason it bites: one `CreateTask` must use exactly ONE connection.
///
/// `create` opens `self.pool.begin()` and then called `allocate_number`, whose
/// first statement ran against `&self.pool` — a SECOND acquire from the same
/// pool while the first was still held. So N concurrent creates against a pool
/// of N take every connection with their transactions and then wait for a
/// connection only one of them can release. Measured at pool size 4 against
/// MariaDB 11.8 before the fix: ONE of four creates succeeded and three failed
/// after the full acquire timeout.
///
/// **A test at the default pool size passes against the broken code**, which is
/// exactly how this survived a suite that already had a concurrency file. Ten
/// connections and two racers always leave a spare, so the second acquire always
/// succeeded and nothing in the repository could see it. The size is NAMED here
/// rather than read from the fixture default, so the ratio cannot quietly widen.
///
/// A FRESH PROJECT PER ROUND, which is the harder case on the other axis too:
/// the counter row does not exist yet, so every racer's allocation races the
/// same new key rather than incrementing a row already there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_many_concurrent_creates_as_the_pool_has_connections_all_succeed() {
    const POOL: u32 = 4;
    let w = Arc::new(World::with_pool_size("td_pool_ratio", POOL).await);

    for round in 0..6 {
        let fresh = format!("acme/at-capacity-{round}");
        let gate = Arc::new(tokio::sync::Barrier::new(POOL as usize));

        let racers: Vec<_> = (0..POOL)
            .map(|n| {
                let w = Arc::clone(&w);
                let gate = Arc::clone(&gate);
                let fresh = fresh.clone();
                tokio::spawn(async move {
                    gate.wait().await;
                    w.try_create(&fresh, U1, &format!("racer {n}")).await
                })
            })
            .collect();

        let mut numbers = Vec::new();
        for racer in racers {
            let created = racer.await.expect("join").unwrap_or_else(|e| {
                panic!(
                    "round {round}: {POOL} concurrent creates against a pool of {POOL} must all \
                     succeed, because one create is one connection: {e}"
                )
            });
            numbers.push(created.number);
        }

        numbers.sort_unstable();
        assert_eq!(
            numbers,
            (1..=POOL).collect::<Vec<_>>(),
            "round {round}: the numbers handed out were not 1..={POOL}"
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
