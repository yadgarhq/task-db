//! ADR-1019: the caller's deadline bounds this module's pool acquire, and a
//! handler dropped at that deadline leaves the pool clean.
//!
//! **WHY BOTH HALVES SHIP TOGETHER.** Once `task` sends `grpc-timeout`, tonic's
//! server drops a handler future at the deadline wherever it is awaiting. The
//! first half makes this module answer `DEADLINE_EXCEEDED` itself, from the
//! acquire, before tonic gives up on it. The second half is what makes the drop
//! safe: sqlx 0.9 counts a transaction only after `BEGIN`'s answer is read
//! (`sqlx-mysql-0.9.0/src/transaction.rs`), and a `Transaction` dropped at depth
//! zero queues no `ROLLBACK`. A handler dropped while `BEGIN` is on the wire
//! returns a connection the ENGINE holds in a transaction and sqlx believes is
//! idle. The release `ping` drains the answer and rolls nothing back.

mod support;

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;
use std::time::{Duration, Instant};

use sqlx::Row as _;
use support::{World, P_A, U1};
use tonic::{Code, Request, Status};
use yadgar_dial::Deadline;
use yadgar_task_db::pb::yadgar::common::v1::Idempotency;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;
use yadgar_task_db::txn;

/// Far above every deadline the tests below send, so a test that answers in
/// time can only have been answered by the deadline.
const CEILING: Duration = Duration::from_secs(10);
const CALLER: Duration = Duration::from_millis(300);

async fn in_transaction(world: &World) -> u64 {
    let mut conn = world.pool.acquire().await.expect("acquire");
    sqlx::query("SELECT @@in_transaction AS t")
        .fetch_one(&mut *conn)
        .await
        .expect("select")
        .try_get::<u64, _>("t")
        .expect("column")
}

#[tokio::test]
async fn a_begin_dropped_on_the_wire_leaves_the_next_borrower_outside_any_transaction() {
    let world = World::exhaustible("td_deadline_begin_drop", CEILING).await;
    assert_eq!(in_transaction(&world).await, 0, "the fixture starts clean");

    let mut held = txn::acquire(&world.pool, Deadline::none())
        .await
        .expect("acquire");
    {
        // ONE POLL: `BEGIN` is written to the socket and the future waits for
        // the engine's answer. Dropping it there is exactly what tonic does to
        // a handler whose `grpc-timeout` expires mid-`BEGIN`.
        let mut begin = pin!(held.begin());
        let first = poll_fn(|cx| Poll::Ready(begin.as_mut().poll(cx))).await;
        assert!(
            first.is_pending(),
            "BEGIN answered inside one poll; the drop below tests nothing"
        );
    }
    drop(held);

    // One connection wide, so this borrower gets whatever the dropped handler
    // left behind — or its replacement.
    assert_eq!(
        in_transaction(&world).await,
        0,
        "the connection went back to the pool inside the dropped handler's transaction"
    );

    // A fresh transaction on the next borrower still opens and closes normally.
    let mut held = txn::acquire(&world.pool, Deadline::none())
        .await
        .expect("acquire");
    let tx = held.begin().await.expect("begin");
    tx.commit().await.expect("commit");
    drop(held);
    assert_eq!(in_transaction(&world).await, 0);
}

fn scope(world: &World) -> Option<yadgar_task_db::pb::yadgar::common::v1::Scope> {
    world.scope(P_A, U1)
}

fn with_deadline<T>(message: T, timeout: Option<Duration>) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(timeout) = timeout {
        request.set_timeout(timeout);
    }
    request
}

/// Every RPC, so each acquire site is driven: two read arms and a list on the
/// read side, the three `begin` sites on the write side.
async fn every_rpc(
    world: &World,
    timeout: Option<Duration>,
) -> Vec<(&'static str, Status, Duration)> {
    let mut seen = Vec::new();
    macro_rules! run {
        ($name:literal, $call:expr) => {{
            let started = Instant::now();
            let status = $call
                .await
                .expect_err(concat!($name, " answered with the pool held"));
            seen.push(($name, status, started.elapsed()));
        }};
    }
    run!(
        "CreateTask",
        world.db.create_task(with_deadline(
            CreateTaskRequest {
                scope: scope(world),
                task: Some(Task {
                    title: "t".into(),
                    status: TaskStatus::Open as i32,
                    ..Default::default()
                }),
                idempotency: None,
            },
            timeout
        ))
    );
    run!(
        "GetTask(id)",
        world.db.get_task(with_deadline(
            GetTaskRequest {
                scope: scope(world),
                key: Some(get_task_request::Key::Id("yadgar:task:x".into())),
            },
            timeout
        ))
    );
    run!(
        "GetTask(number)",
        world.db.get_task(with_deadline(
            GetTaskRequest {
                scope: scope(world),
                key: Some(get_task_request::Key::Number(1)),
            },
            timeout
        ))
    );
    run!(
        "ListTasks",
        world.db.list_tasks(with_deadline(
            ListTasksRequest {
                scope: scope(world),
                ..Default::default()
            },
            timeout
        ))
    );
    run!(
        "UpdateTask",
        world.db.update_task(with_deadline(
            UpdateTaskRequest {
                scope: scope(world),
                id: "yadgar:task:x".into(),
                expect_version: 1,
                task: Some(Task {
                    title: "t".into(),
                    ..Default::default()
                }),
                update_mask: Some(prost_types::FieldMask {
                    paths: vec!["title".into()],
                }),
                caller_fingerprint: vec![],
                idempotency: None,
            },
            timeout
        ))
    );
    run!(
        "DeleteTask",
        world.db.delete_task(with_deadline(
            DeleteTaskRequest {
                scope: scope(world),
                id: "yadgar:task:x".into(),
                expect_version: 1,
                caller_fingerprint: vec![],
                idempotency: None,
            },
            timeout
        ))
    );
    // A READ with no transaction, but it still takes a connection, so the
    // caller's deadline bounds its acquire exactly as it bounds the others.
    run!(
        "LookupWrite",
        world.db.lookup_write(with_deadline(
            LookupWriteRequest {
                scope: scope(world),
                idempotency: Some(Idempotency { key: "k".into() }),
                rpc: WriteRpc::UpdateTask as i32,
                caller_fingerprint: vec![1; 32],
            },
            timeout
        ))
    );
    seen
}

#[tokio::test]
async fn an_exhausted_pool_answers_deadline_exceeded_inside_the_callers_deadline() {
    let world = World::exhaustible("td_deadline_exhausted", CEILING).await;
    let _only = world
        .pool
        .acquire()
        .await
        .expect("hold the only connection");

    for (rpc, status, elapsed) in every_rpc(&world, Some(CALLER)).await {
        assert_eq!(
            status.code(),
            Code::DeadlineExceeded,
            "{rpc} answered {status:?} instead of DEADLINE_EXCEEDED"
        );
        assert!(
            elapsed < CALLER,
            "{rpc} took {elapsed:?}, not inside the caller's {CALLER:?}: the acquire waited \
             toward its own {CEILING:?} ceiling"
        );
    }
}

#[tokio::test]
async fn without_a_deadline_the_acquire_still_fails_at_its_own_ceiling() {
    let ceiling = Duration::from_millis(400);
    let world = World::exhaustible("td_deadline_none", ceiling).await;
    let _only = world
        .pool
        .acquire()
        .await
        .expect("hold the only connection");

    for (rpc, status, elapsed) in every_rpc(&world, None).await {
        // Today's answer for a pool that stayed full: UNAVAILABLE, retryable.
        assert_eq!(status.code(), Code::Unavailable, "{rpc}: {status:?}");
        assert!(
            elapsed >= ceiling,
            "{rpc} gave up after {elapsed:?}, before its {ceiling:?} ceiling"
        );
    }
}

#[tokio::test]
async fn a_deadline_longer_than_the_ceiling_never_raises_it() {
    let ceiling = Duration::from_millis(400);
    let world = World::exhaustible("td_deadline_long", ceiling).await;
    let _only = world
        .pool
        .acquire()
        .await
        .expect("hold the only connection");

    for (rpc, status, elapsed) in every_rpc(&world, Some(Duration::from_secs(30))).await {
        assert_eq!(status.code(), Code::Unavailable, "{rpc}: {status:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "{rpc} waited {elapsed:?}: the caller's 30s raised the {ceiling:?} ceiling"
        );
    }
}

#[tokio::test]
async fn a_deadline_already_spent_answers_without_touching_the_pool() {
    let world = World::exhaustible("td_deadline_spent", CEILING).await;
    // The pool is FREE: a spent budget refuses on its own, not because the
    // acquire timed out.
    let started = Instant::now();
    let Err(err) = txn::acquire(&world.pool, Deadline::after(Duration::from_millis(10))).await
    else {
        panic!("a 10ms budget is inside the 50ms margin, and the acquire went ahead");
    };
    assert_eq!(err.code(), Code::DeadlineExceeded);
    assert!(started.elapsed() < Duration::from_millis(50));
}
