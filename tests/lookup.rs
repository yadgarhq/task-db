//! `LookupWrite` (ADR-1051): a read of the D9 claim ledger by idempotency key,
//! so the logic service can replay a same-key retry BEFORE it reads the task.
//!
//! The claim is found by equality on `(project_id, user_id, idem_key)` and by
//! nothing else, so the tests here pin both halves of that: another user or
//! another project MISSES, and a task that has since been removed still HITS,
//! because the claim row outlives the task it names.
//!
//! A miss is `OK` with the oneof unset, never an error status. Every refusal is
//! `INVALID_ARGUMENT`, and the request-shape refusals are proved to come BEFORE
//! any database access by sending them to a service whose pool is closed.

mod support;

use prost::Message as _;
use support::{World, P_A, P_B, U1, U2};
use tonic::{Code, Request, Status};
use yadgar_task_db::pb::yadgar::common::v1::{Idempotency, Meta, Scope};
use yadgar_task_db::pb::yadgar::task::v1::lookup_write_response::Outcome;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

/// The two texts a colliding write claim already answers with (`src/idem.rs`).
/// Matched on a fragment each, because the point is WHICH refusal arrived.
const DIFFERENT_OPERATION: &str = "already used for a different operation";
const DIFFERENT_REQUEST: &str = "already used for a different request";

fn key(k: &str) -> Option<Idempotency> {
    Some(Idempotency { key: k.into() })
}

/// A caller fingerprint: 32 bytes, distinguishable by `n`.
fn fp(n: u8) -> Vec<u8> {
    vec![n; 32]
}

fn a_task(title: &str) -> Option<Task> {
    Some(Task {
        title: title.into(),
        body: "b".into(),
        status: TaskStatus::Open as i32,
        ..Default::default()
    })
}

async fn lookup(
    w: &World,
    scope: Option<Scope>,
    idempotency: Option<Idempotency>,
    rpc: i32,
    caller_fingerprint: Vec<u8>,
) -> Result<LookupWriteResponse, Status> {
    w.db.lookup_write(Request::new(LookupWriteRequest {
        scope,
        idempotency,
        rpc,
        caller_fingerprint,
    }))
    .await
    .map(|r| r.into_inner())
}

/// A lookup in P_A as U1 of `k`, for `rpc`, carrying `fp(n)`.
async fn lookup_as_u1(w: &World, k: &str, rpc: WriteRpc, n: u8) -> Result<Option<Outcome>, Status> {
    lookup(w, w.scope(P_A, U1), key(k), rpc as i32, fp(n))
        .await
        .map(|r| r.outcome)
}

async fn update(
    w: &World,
    id: &str,
    title: &str,
    k: &str,
    caller_fingerprint: Vec<u8>,
) -> Result<UpdateTaskResponse, Status> {
    w.db.update_task(Request::new(UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.into(),
        expect_version: 1,
        task: a_task(title),
        update_mask: None,
        caller_fingerprint,
        idempotency: key(k),
    }))
    .await
    .map(|r| r.into_inner())
}

fn refused_with(result: Result<Option<Outcome>, Status>, text: &str) {
    let err = result.expect_err("this lookup must be refused");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(
        err.message().contains(text),
        "expected the text containing {text:?}, got {:?}",
        err.message()
    );
}

#[tokio::test]
async fn a_key_never_claimed_is_ok_with_the_outcome_unset() {
    let w = World::fresh("td_lookup_miss").await;
    for rpc in [
        WriteRpc::CreateTask,
        WriteRpc::UpdateTask,
        WriteRpc::DeleteTask,
    ] {
        assert_eq!(
            lookup_as_u1(&w, "never-used", rpc, 1)
                .await
                .expect("a miss is OK, never an error status"),
            None,
            "a miss leaves the oneof unset ({rpc:?})"
        );
    }
}

/// The recorded outcome, BYTE-EQUAL to what the write returned, and taken from
/// the claim rather than from the task row: the row is edited again (keyless)
/// before the lookup, so an outcome recomputed from the row would carry
/// version 3 and the recorded one carries 2.
#[tokio::test]
async fn a_hit_returns_the_update_exactly_as_recorded() {
    let w = World::fresh("td_lookup_hit_update").await;
    let id = w.create(P_A, U1, "before").await;
    let first = update(&w, &id, "after", "k-hit", fp(1))
        .await
        .expect("first");

    w.db.update_task(Request::new(UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 2,
        task: a_task("moved on since"),
        update_mask: None,
        caller_fingerprint: vec![],
        idempotency: None,
    }))
    .await
    .expect("a later keyless edit");

    let Some(Outcome::Update(recorded)) = lookup_as_u1(&w, "k-hit", WriteRpc::UpdateTask, 1)
        .await
        .expect("hit")
    else {
        panic!("a hit for UPDATE_TASK is set in the `update` arm");
    };
    assert_eq!(recorded, first);
    assert_eq!(
        recorded.encode_to_vec(),
        first.encode_to_vec(),
        "byte-equal to the write's own response"
    );
    assert_eq!(
        recorded.previous_status,
        TaskStatus::Open as i32,
        "previous_status is part of what was recorded"
    );
}

#[tokio::test]
async fn a_hit_returns_a_recorded_create_and_a_recorded_delete_in_their_own_arms() {
    let w = World::fresh("td_lookup_hit_arms").await;
    let created =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U1),
            task: a_task("t"),
            idempotency: key("k-create"),
        }))
        .await
        .expect("create")
        .into_inner();
    // A CreateTask claim stores no caller fingerprint, so any 32 bytes replay.
    assert_eq!(
        lookup_as_u1(&w, "k-create", WriteRpc::CreateTask, 5)
            .await
            .expect("hit"),
        Some(Outcome::Create(created.clone()))
    );

    w.db.delete_task(Request::new(DeleteTaskRequest {
        scope: w.scope(P_A, U1),
        id: created.meta.expect("meta").id,
        expect_version: 1,
        caller_fingerprint: fp(3),
        idempotency: key("k-delete"),
    }))
    .await
    .expect("delete");
    assert_eq!(
        lookup_as_u1(&w, "k-delete", WriteRpc::DeleteTask, 3)
            .await
            .expect("hit"),
        Some(Outcome::Delete(DeleteTaskResponse {}))
    );
}

/// The key is CLIENT-supplied, so another user holding the same string must
/// never be handed this user's outcome — and the same user in another project
/// has never claimed it there either.
#[tokio::test]
async fn another_users_key_and_another_projects_key_are_misses() {
    let w = World::fresh("td_lookup_scoped").await;
    let id = w.create(P_A, U1, "before").await;
    update(&w, &id, "after", "shared-key", fp(1))
        .await
        .expect("u1 claims the key");

    for (project, user) in [(P_A, U2), (P_B, U1)] {
        let found = lookup(
            &w,
            w.scope(project, user),
            key("shared-key"),
            WriteRpc::UpdateTask as i32,
            fp(1),
        )
        .await
        .expect("a miss is OK");
        assert_eq!(found.outcome, None, "{project} / {user} never claimed it");
    }
}

#[tokio::test]
async fn a_hit_naming_another_rpc_is_refused_as_a_different_operation() {
    let w = World::fresh("td_lookup_rpc").await;
    w.db.create_task(Request::new(CreateTaskRequest {
        scope: w.scope(P_A, U1),
        task: a_task("t"),
        idempotency: key("k-created"),
    }))
    .await
    .expect("create");
    let id = w.create(P_A, U1, "u").await;
    update(&w, &id, "after", "k-updated", fp(1))
        .await
        .expect("update");

    refused_with(
        lookup_as_u1(&w, "k-created", WriteRpc::UpdateTask, 1).await,
        DIFFERENT_OPERATION,
    );
    refused_with(
        lookup_as_u1(&w, "k-created", WriteRpc::DeleteTask, 1).await,
        DIFFERENT_OPERATION,
    );
    refused_with(
        lookup_as_u1(&w, "k-updated", WriteRpc::DeleteTask, 1).await,
        DIFFERENT_OPERATION,
    );
    refused_with(
        lookup_as_u1(&w, "k-updated", WriteRpc::CreateTask, 1).await,
        DIFFERENT_OPERATION,
    );
}

#[tokio::test]
async fn a_hit_storing_another_caller_fingerprint_is_refused_as_a_different_request() {
    let w = World::fresh("td_lookup_fp").await;
    let id = w.create(P_A, U1, "before").await;
    update(&w, &id, "after", "k-fp", fp(1))
        .await
        .expect("update");

    refused_with(
        lookup_as_u1(&w, "k-fp", WriteRpc::UpdateTask, 2).await,
        DIFFERENT_REQUEST,
    );
}

/// A claim whose write carried no caller fingerprint — every claim before
/// migration 8, and every write from a task that predates ADR-1051 — has
/// nothing to compare against, so a lookup for the same rpc REPLAYS.
#[tokio::test]
async fn a_claim_storing_no_caller_fingerprint_replays_for_the_same_rpc() {
    let w = World::fresh("td_lookup_null").await;
    let id = w.create(P_A, U1, "before").await;
    let first = update(&w, &id, "after", "k-null", vec![])
        .await
        .expect("an update sent without a caller fingerprint");
    assert_eq!(
        w.stored_caller_fingerprint(P_A, U1, "k-null").await,
        None,
        "an empty caller fingerprint is ABSENT and stores NULL, not 32 zeros"
    );

    assert_eq!(
        lookup_as_u1(&w, "k-null", WriteRpc::UpdateTask, 9)
            .await
            .expect("hit"),
        Some(Outcome::Update(first))
    );
}

/// THE CASE ADR-1051 EXISTS FOR. The task the claim names has been removed,
/// so the logic service's read would answer NOT_FOUND — and the lookup still
/// hits, because it never looks at the task.
#[tokio::test]
async fn a_lookup_still_hits_after_the_task_is_soft_deleted() {
    let w = World::fresh("td_lookup_after_delete").await;
    let id = w.create(P_A, U1, "before").await;
    let first = update(&w, &id, "after", "k-edit", fp(1))
        .await
        .expect("update");

    w.db.delete_task(Request::new(DeleteTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 2,
        caller_fingerprint: fp(2),
        idempotency: key("k-remove"),
    }))
    .await
    .expect("delete");
    assert_eq!(
        w.read(P_A, U1, &id).await.expect_err("gone").code(),
        Code::NotFound,
        "the precondition: the task is no longer readable"
    );

    assert_eq!(
        lookup_as_u1(&w, "k-edit", WriteRpc::UpdateTask, 1)
            .await
            .expect("hit"),
        Some(Outcome::Update(first))
    );
    assert_eq!(
        lookup_as_u1(&w, "k-remove", WriteRpc::DeleteTask, 2)
            .await
            .expect("hit"),
        Some(Outcome::Delete(DeleteTaskResponse {}))
    );
}

/// Every request-shape refusal, sent to a service whose POOL IS CLOSED. A
/// refusal that came after a database access would answer with the pool's
/// error instead of INVALID_ARGUMENT, so this is what proves the order.
#[tokio::test]
async fn a_malformed_lookup_is_invalid_argument_before_any_database_access() {
    let w = World::fresh("td_lookup_shape").await;
    w.pool.close().await;
    let scope = w.scope(P_A, U1);
    let update = WriteRpc::UpdateTask as i32;

    let cases: Vec<(&str, LookupWriteRequest)> = vec![
        (
            "no idempotency",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: None,
                rpc: update,
                caller_fingerprint: fp(1),
            },
        ),
        (
            "an empty key",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: key(""),
                rpc: update,
                caller_fingerprint: fp(1),
            },
        ),
        (
            "WRITE_RPC_UNSPECIFIED",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: key("k"),
                rpc: WriteRpc::Unspecified as i32,
                caller_fingerprint: fp(1),
            },
        ),
        (
            "an rpc outside the enum",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: key("k"),
                rpc: 99,
                caller_fingerprint: fp(1),
            },
        ),
        (
            "an empty caller fingerprint",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: key("k"),
                rpc: update,
                caller_fingerprint: vec![],
            },
        ),
        (
            "a 31-byte caller fingerprint",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: key("k"),
                rpc: update,
                caller_fingerprint: vec![1; 31],
            },
        ),
        (
            "a 33-byte caller fingerprint",
            LookupWriteRequest {
                scope: scope.clone(),
                idempotency: key("k"),
                rpc: update,
                caller_fingerprint: vec![1; 33],
            },
        ),
        (
            "no scope",
            LookupWriteRequest {
                scope: None,
                idempotency: key("k"),
                rpc: update,
                caller_fingerprint: fp(1),
            },
        ),
    ];
    for (what, req) in cases {
        let err = w.db.lookup_write(Request::new(req)).await.expect_err(what);
        assert_eq!(err.code(), Code::InvalidArgument, "{what}: {err:?}");
    }

    // The control: a WELL-FORMED lookup on the same closed pool does reach the
    // database and fails there, with something other than INVALID_ARGUMENT.
    // Without it, every case above could pass for a handler that refuses
    // everything.
    let reached = lookup(&w, scope, key("k"), update, fp(1))
        .await
        .expect_err("a closed pool cannot answer a well-formed lookup");
    assert_ne!(reached.code(), Code::InvalidArgument, "{reached:?}");
}

/// The recorded outcome is decoded, never recomputed — so a claim row this
/// build did not write is returned as it stands. Seeded with values no build
/// would generate.
#[tokio::test]
async fn a_hit_decodes_the_stored_response_rather_than_recomputing_it() {
    let w = World::fresh("td_lookup_seeded").await;
    let recorded = UpdateTaskResponse {
        meta: Some(Meta {
            id: "yadgar:task:never-in-the-task-table".into(),
            version: 4242,
            project_id: P_A.into(),
            ..Default::default()
        }),
        previous_status: TaskStatus::Blocked as i32,
    };
    sqlx::query(
        "INSERT INTO task_write (project_id, user_id, idem_key, rpc, response)
         VALUES (?, ?, ?, 'UpdateTask', ?)",
    )
    .bind(P_A)
    .bind(U1)
    .bind("k-seeded")
    .bind(recorded.encode_to_vec())
    .execute(&w.pool)
    .await
    .expect("seed");

    assert_eq!(
        lookup_as_u1(&w, "k-seeded", WriteRpc::UpdateTask, 1)
            .await
            .expect("hit"),
        Some(Outcome::Update(recorded))
    );
}
