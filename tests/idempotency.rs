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
//! The file holds both halves, and telling them apart is the whole point. The
//! replay tests in the first half send the SAME payload twice, which is the
//! case D9 always covered; the refusal tests in the second half send a
//! different one under a used key. Do not read
//! `a_replayed_create_returns_the_original_and_writes_once` as contradicting
//! amended D9 — it asserts the same-payload case, which the amendment leaves
//! untouched, and "fixing" the contradiction by deleting it would be wrong.
//!
//! Two tests look like the refusal and are not, so neither should be
//! "corrected" into one:
//!
//! - `a_failed_write_leaves_no_claim_behind` reuses `k-fail` under `UpdateTask`
//!   with a genuinely DIFFERENT payload (`expect_version: 99` then `1`, "nope"
//!   then "corrected") and the second call succeeds. The first call's failure
//!   rolls back and leaves no committed claim, so the second finds no repeated
//!   key and never reaches `replay` at all.
//! - `a_reordered_update_mask_is_still_a_replay` sends different BYTES under a
//!   used key and replays. A mask is a set of field names, so a reordering
//!   discards nothing, and `Payload for UpdateTaskRequest` sorts the paths
//!   before the digest is taken.
//!
//! `a_claim_recorded_before_the_fingerprint_still_replays` covers the third
//! case: an absent digest is not a differing one. See `src/idem.rs` for which
//! fields count as the payload, stated per RPC.

mod support;

use prost::Message as _;
use support::{World, P_A, U1, U2};
use tonic::Request;
use yadgar_task_db::pb::yadgar::common::v1::{Idempotency, Meta};
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

// ---------------------------------------------------------------------------
// D9's amendment: a repeated key carrying a DIFFERENT payload is a refusal, not
// a replay. Replaying it hands the first request's outcome to a caller who sent
// a second — the operation actually asked for is discarded and the answer says
// success.
// ---------------------------------------------------------------------------

/// Every assertion below the status code is the one that matters. A refusal
/// that still wrote the substituted row, or that lost the original, would
/// return `INVALID_ARGUMENT` and be wrong.
#[tokio::test]
async fn a_key_reused_with_a_different_create_is_refused() {
    let w = World::fresh("td_idem_differing_create").await;

    w.db.create_task(Request::new(CreateTaskRequest {
        scope: w.scope(P_A, U1),
        task: a_task("the one that was asked for"),
        idempotency: key("k-differ"),
    }))
    .await
    .expect("first");

    let err =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U1),
            task: a_task("a different request under the same key"),
            idempotency: key("k-differ"),
        }))
        .await
        .expect_err("a differing payload under a used key is refused, not replayed");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        w.count_titled("the one that was asked for").await,
        1,
        "the refusal must not disturb the write that already happened"
    );
    assert_eq!(
        w.count_titled("a different request under the same key")
            .await,
        0,
        "and must not perform the second one either"
    );
}

/// The update path, where a silent replay is worse still: the caller is told
/// version 2 exists and believes its own title is what reached the row.
#[tokio::test]
async fn a_key_reused_with_a_different_update_is_refused() {
    let w = World::fresh("td_idem_differing_update").await;
    let id = w.create(P_A, U1, "before").await;

    let write = |title: &str| UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 1,
        task: a_task(title),
        update_mask: None,
        idempotency: key("k-differ-update"),
    };

    w.db.update_task(Request::new(write("what the first call wrote")))
        .await
        .expect("first");

    let err =
        w.db.update_task(Request::new(write("what the second call wanted")))
            .await
            .expect_err("a differing payload under a used key is refused, not replayed");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").title,
        "what the first call wrote",
        "the row still holds what the call that succeeded wrote"
    );
}

/// A mask naming the same fields in another order asks for the same write, so
/// it is a replay. The paths are sorted before the fingerprint is taken for
/// exactly this: D9's test is whether a replay would silently DISCARD the
/// difference, and a reordered mask discards nothing.
#[tokio::test]
async fn a_reordered_update_mask_is_still_a_replay() {
    let w = World::fresh("td_idem_mask_order").await;
    let id = w.create(P_A, U1, "before").await;

    let write = |paths: Vec<&str>| UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 1,
        task: a_task("after"),
        update_mask: Some(prost_types::FieldMask {
            paths: paths.into_iter().map(String::from).collect(),
        }),
        idempotency: key("k-mask"),
    };

    let first =
        w.db.update_task(Request::new(write(vec!["title", "body"])))
            .await
            .expect("first")
            .into_inner();
    let replay =
        w.db.update_task(Request::new(write(vec!["body", "title"])))
            .await
            .expect("the same fields in another order is the same request")
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

/// A claim recorded before the fingerprint column existed cannot be compared
/// against anything, and an absent fingerprint means REPLAY rather than refuse.
///
/// The alternative fails the rule it implements: a `NOT NULL DEFAULT ''` column
/// would make an identical retry of a pre-migration key mismatch the empty
/// string and be refused, which is D9's core case regressed by the change that
/// implements D9's amendment.
///
/// Written by hand against a database stopped short of the migration, for the
/// reason `World::fresh_at` exists: today's `claim` binds the new column, so
/// the service cannot produce the BEFORE state this is about. The seeded
/// response carries values no build would generate, so the replay cannot be
/// passing because the module happened to compute the same answer.
#[tokio::test]
async fn a_claim_recorded_before_the_fingerprint_still_replays() {
    let w = World::fresh_at("td_idem_legacy", 5).await;

    let recorded = CreateTaskResponse {
        meta: Some(Meta {
            id: "yadgar:task:written-before-the-column".into(),
            version: 1,
            project_id: P_A.into(),
            owner_user_id: U1.into(),
            ..Default::default()
        }),
        number: 4242,
    };
    sqlx::query(
        "INSERT INTO task_write (project_id, user_id, idem_key, rpc, response)
         VALUES (?, ?, ?, 'CreateTask', ?)",
    )
    .bind(P_A)
    .bind(U1)
    .bind("k-legacy")
    .bind(recorded.encode_to_vec())
    .execute(&w.pool)
    .await
    .expect("seed the row an older build wrote");

    assert_eq!(
        w.migrate_to_head().await,
        6,
        "the fixture stopped at 5, so the fingerprint migration is what ran"
    );

    let replay =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U1),
            task: a_task("whatever this build sends"),
            idempotency: key("k-legacy"),
        }))
        .await
        .expect("a claim with no fingerprint cannot be compared, so it replays")
        .into_inner();

    assert_eq!(
        replay, recorded,
        "the ORIGINAL outcome, which this test chose rather than the module"
    );
    assert_eq!(
        w.count_titled("whatever this build sends").await,
        0,
        "a replay writes nothing"
    );
}
