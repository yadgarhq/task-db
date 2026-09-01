//! D9: a repeated key is a REPLAY, never a failure — for the identical retry
//! the rule was written for. As amended, D9 also refuses a repeated key
//! carrying a DIFFERENT payload, with `INVALID_ARGUMENT`, rather than
//! replaying it.
//!
//! Behind a retrying load balancer a write fires more than once, and the second
//! delivery must return the first one's outcome rather than performing the write
//! again or reporting an error. Reporting an error is the worse of the two:
//! under D39 it tells an instance its write failed when the record exists, which
//! it then "rectifies" by writing a duplicate.
//!
//! Every replay test below sends the SAME payload twice, which is the case D9
//! always covered and this module already handles. None of them exercises the
//! differing-payload case the amendment adds: `task_write` stores the prior
//! response, not the prior request, so this module cannot yet compare payloads,
//! and `a_key_reused_for_a_different_operation_is_refused` still only refuses a
//! reused key across a DIFFERENT rpc, not a differing payload under the same
//! one. Do not read `a_replayed_create_returns_the_original_and_writes_once` as
//! contradicting amended D9 — it asserts the same-payload case, which the
//! amendment leaves untouched, and "fixing" the contradiction by deleting it
//! would be wrong. Watch `a_failed_write_leaves_no_claim_behind` more closely:
//! it reuses `k-fail` under `UpdateTask` with a genuinely DIFFERENT payload
//! (`expect_version: 99` then `expect_version: 1`, "nope" then "corrected") and
//! the second call succeeds — surface-identical to what the amendment refuses.
//! It is not that case: the first call's failure leaves no committed claim, so
//! the second finds no repeated key to refuse and never reaches `replay` at
//! all. The gap is known, not an oversight, and it is booked as O21 in the
//! decision record; see `src/idem.rs` for where the comparison would have to
//! live.

mod support;

use support::{World, P_A, U1, U2};
use tonic::Request;
use yadgar_task_db::pb::yadgar::common::v1::Idempotency;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

fn key(k: &str) -> Option<Idempotency> {
    Some(Idempotency { key: k.into() })
}

fn a_task(title: &str) -> Option<Task> {
    Some(Task {
        title: title.into(),
        body: "b".into(),
        status: TaskStatus::Open as i32,
        ..Default::default()
    })
}

#[tokio::test]
async fn a_replayed_create_returns_the_original_and_writes_once() {
    let w = World::fresh("td_idem_create").await;
    let req = || CreateTaskRequest {
        scope: w.scope(P_A, U1),
        task: a_task("retried"),
        idempotency: key("k-create"),
    };

    let first =
        w.db.create_task(Request::new(req()))
            .await
            .expect("first")
            .into_inner();
    let replay =
        w.db.create_task(Request::new(req()))
            .await
            .expect("a replay is not a failure")
            .into_inner();

    assert_eq!(first, replay, "the replay must return the ORIGINAL outcome");
    assert_eq!(
        w.count_titled("retried").await,
        1,
        "the replay wrote a second row"
    );
}

/// The sharpest of the three. Without idempotency the retry does not merely
/// write twice — it fails, because the first write already moved the version
/// past the `expect_version` the retry still carries.
#[tokio::test]
async fn a_replayed_update_returns_the_original_outcome() {
    let w = World::fresh("td_idem_update").await;
    let id = w.create(P_A, U1, "before").await;

    let req = || UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 1,
        task: a_task("after"),
        update_mask: None,
        idempotency: key("k-update"),
    };

    let first =
        w.db.update_task(Request::new(req()))
            .await
            .expect("first")
            .into_inner();
    let replay =
        w.db.update_task(Request::new(req()))
            .await
            .expect("a replayed update is a replay, not a stale writer")
            .into_inner();

    assert_eq!(first, replay);
    assert_eq!(
        w.read(P_A, U1, &id)
            .await
            .expect("read")
            .meta
            .expect("meta")
            .version,
        2,
        "the replay applied the update a second time"
    );
}

#[tokio::test]
async fn a_replayed_delete_is_not_an_error() {
    let w = World::fresh("td_idem_delete").await;
    let id = w.create(P_A, U1, "doomed").await;

    let req = || DeleteTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 1,
        idempotency: key("k-delete"),
    };

    w.db.delete_task(Request::new(req())).await.expect("first");
    w.db.delete_task(Request::new(req()))
        .await
        .expect("a replayed delete returns the original outcome");
}

/// The key is CLIENT-supplied, so two clients will eventually pick the same
/// string. Deduplicating on the key alone would hand one user another's record.
#[tokio::test]
async fn the_same_key_from_another_user_is_a_different_write() {
    let w = World::fresh("td_idem_scoped").await;

    let mine =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U1),
            task: a_task("mine"),
            idempotency: key("collision"),
        }))
        .await
        .expect("u1")
        .into_inner();
    let theirs =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U2),
            task: a_task("theirs"),
            idempotency: key("collision"),
        }))
        .await
        .expect("u2 is not replaying u1's write")
        .into_inner();

    assert_ne!(mine.meta.expect("meta").id, theirs.meta.expect("meta").id);
    assert_eq!(w.count_titled("theirs").await, 1);
}

/// A key reused across operations is a client bug, and the stored outcome is the
/// wrong message type to return. Refused loudly rather than decoded hopefully.
#[tokio::test]
async fn a_key_reused_for_a_different_operation_is_refused() {
    let w = World::fresh("td_idem_crossed").await;
    let id =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U1),
            task: a_task("t"),
            idempotency: key("reused"),
        }))
        .await
        .expect("create")
        .into_inner()
        .meta
        .expect("meta")
        .id;

    let err =
        w.db.delete_task(Request::new(DeleteTaskRequest {
            scope: w.scope(P_A, U1),
            id,
            expect_version: 1,
            idempotency: key("reused"),
        }))
        .await
        .expect_err("the same key naming a different operation must be refused");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// An absent key is the honest "this caller does not participate" case, and must
/// not collapse every keyless write into one deduplicated slot.
#[tokio::test]
async fn writes_without_a_key_are_not_deduplicated() {
    let w = World::fresh("td_idem_absent").await;
    w.create(P_A, U1, "same").await;
    w.create(P_A, U1, "same").await;
    assert_eq!(w.count_titled("same").await, 2);
}

/// A write that FAILS records nothing, so the retry is a fresh attempt rather
/// than a replay of an outcome that never happened.
#[tokio::test]
async fn a_failed_write_leaves_no_claim_behind() {
    let w = World::fresh("td_idem_failed").await;
    let id = w.create(P_A, U1, "t").await;

    let bad = UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 99,
        task: a_task("nope"),
        update_mask: None,
        idempotency: key("k-fail"),
    };
    w.db.update_task(Request::new(bad))
        .await
        .expect_err("version 99 does not exist");

    w.db.update_task(Request::new(UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 1,
        task: a_task("corrected"),
        update_mask: None,
        idempotency: key("k-fail"),
    }))
    .await
    .expect("the corrected retry must not replay a failure");
}
