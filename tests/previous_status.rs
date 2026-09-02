//! `UpdateTaskResponse.previous_status` — the status the row held BEFORE the
//! update was applied, recorded at the time of the write and returned unchanged
//! on every replay of the same key (D9).
//!
//! **THE FIXTURES NEVER USE A VALUE THE IMPLEMENTATION COULD HAVE CHOSEN.**
//! Every test here moves the row to `TASK_STATUS_DROPPED` first and then
//! updates it to something else. `DROPPED` is not the zero value, not what
//! `World::create` seeds, and not what any update under test writes — so an
//! assertion that reads `DROPPED` back can only have come from the row as it
//! stood beforehand. Asserting `OPEN` would pass for an implementation that
//! echoed the seeded status, and asserting the NEW status would pass for one
//! that read the row after the write.
//!
//! Three properties, and each is proved by a mutation recorded in the PR:
//!
//! - the value is the one the write DISPLACED — mutated by reading the status
//!   after the `UPDATE` instead of before it;
//! - it is recorded on EVERY update, not only when the mask names `status` —
//!   mutated by emitting `TASK_STATUS_UNSPECIFIED` when the mask omits it;
//! - a replay returns what the FIRST attempt displaced, never a value
//!   recomputed from a row that has since moved — mutated by recomputing.

mod support;

use support::{World, P_A, U1};
use tonic::Request;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

/// The status every test here parks the row at before the update under test.
/// See the module comment for why it is this one and not `OPEN`.
const PARKED: TaskStatus = TaskStatus::Dropped;

fn mask(paths: &[&str]) -> Option<prost_types::FieldMask> {
    Some(prost_types::FieldMask {
        paths: paths.iter().map(|p| (*p).to_string()).collect(),
    })
}

fn request(
    w: &World,
    id: &str,
    expect_version: u64,
    title: &str,
    status: TaskStatus,
    paths: &[&str],
    key: Option<&str>,
) -> UpdateTaskRequest {
    UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.into(),
        expect_version,
        task: Some(Task {
            title: title.into(),
            body: "b".into(),
            status: status as i32,
            ..Default::default()
        }),
        update_mask: mask(paths),
        idempotency: key
            .map(|k| yadgar_task_db::pb::yadgar::common::v1::Idempotency { key: k.into() }),
    }
}

async fn update(w: &World, req: UpdateTaskRequest) -> UpdateTaskResponse {
    w.db.update_task(Request::new(req))
        .await
        .expect("update")
        .into_inner()
}

/// Create a task and park it at [`PARKED`]. Answers with its id; the row is at
/// version 2, because the parking move is itself an update.
async fn parked(w: &World, title: &str) -> String {
    let id = w.create(P_A, U1, title).await;
    let response = update(w, request(w, &id, 1, title, PARKED, &["status"], None)).await;
    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").status,
        PARKED as i32,
        "the fixture did not park the row, so nothing below tests what it says"
    );
    assert_eq!(
        response.previous_status,
        TaskStatus::Open as i32,
        "the parking move displaced the status `create` seeded"
    );
    id
}

/// The core property. `DONE` is written over `DROPPED`, and the response must
/// name `DROPPED`.
#[tokio::test]
async fn an_update_reports_the_status_the_row_held_before_it() {
    let w = World::fresh("td_prev_displaced").await;
    let id = parked(&w, "t").await;

    let response = update(
        &w,
        request(&w, &id, 2, "t", TaskStatus::Done, &["status"], None),
    )
    .await;

    assert_eq!(
        response.previous_status, PARKED as i32,
        "previous_status must be the status the update displaced"
    );
    assert_ne!(
        response.previous_status,
        TaskStatus::Done as i32,
        "reporting the POST-update status is the defect this field exists to close"
    );
    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").status,
        TaskStatus::Done as i32,
        "the update itself still applied"
    );
}

/// The obligation the field's own comment pins: recorded on EVERY update, not
/// only when the status changes. A mask that omits `status` must not become a
/// second cause of `TASK_STATUS_UNSPECIFIED`.
#[tokio::test]
async fn an_update_that_does_not_name_status_still_reports_the_prior_status() {
    let w = World::fresh("td_prev_mask_omits_status").await;
    let id = parked(&w, "t").await;

    // `status` is deliberately set to a THIRD value in the request body. The
    // mask does not name it, so it must not be written — and it must not be
    // what comes back either.
    let response = update(
        &w,
        request(
            &w,
            &id,
            2,
            "renamed",
            TaskStatus::InProgress,
            &["title"],
            None,
        ),
    )
    .await;

    assert_ne!(
        response.previous_status,
        TaskStatus::Unspecified as i32,
        "an update_mask that omits `status` must not become a second cause of \
         TASK_STATUS_UNSPECIFIED — after this field ships the only legitimate \
         cause is an idempotency row written before it existed"
    );
    assert_eq!(
        response.previous_status, PARKED as i32,
        "the row held a status beforehand whether or not the mask named it"
    );

    let after = w.read(P_A, U1, &id).await.expect("read");
    assert_eq!(after.title, "renamed", "the masked field was written");
    assert_eq!(
        after.status, PARKED as i32,
        "the unmasked field was not written, so previous_status equals the \
         status the row still holds — which is the truth, not an omission"
    );
}

/// The case the contract calls out by name: an identity transition passes the
/// rules unchanged, and still displaced a status.
#[tokio::test]
async fn an_identity_transition_reports_the_status_it_did_not_change() {
    let w = World::fresh("td_prev_identity").await;
    let id = parked(&w, "t").await;

    let response = update(&w, request(&w, &id, 2, "t", PARKED, &["status"], None)).await;

    assert_eq!(
        response.previous_status, PARKED as i32,
        "a status written over itself was still displaced by this write"
    );
}

/// The replay guarantee, and the reason the value has to be RECORDED rather
/// than recomputed. After the first attempt the row holds `DONE`, so anything
/// that re-derives the answer on a replay returns `DONE` — the exact assumption
/// `yadgar.taskapi.v1.TransitionTaskResponse.from` was added to break.
#[tokio::test]
async fn a_replayed_update_returns_the_status_the_first_attempt_displaced() {
    let w = World::fresh("td_prev_replay").await;
    let id = parked(&w, "t").await;

    let first = update(
        &w,
        request(
            &w,
            &id,
            2,
            "t",
            TaskStatus::Done,
            &["status"],
            Some("k-prev"),
        ),
    )
    .await;
    assert_eq!(first.previous_status, PARKED as i32);

    let replay = update(
        &w,
        request(
            &w,
            &id,
            2,
            "t",
            TaskStatus::Done,
            &["status"],
            Some("k-prev"),
        ),
    )
    .await;

    assert_eq!(
        replay.previous_status, PARKED as i32,
        "a replay returns what the FIRST attempt displaced"
    );
    assert_ne!(
        replay.previous_status,
        TaskStatus::Done as i32,
        "the row holds DONE by now, so this is what a recomputed answer would say"
    );
    assert_eq!(
        replay.meta.expect("meta").version,
        first.meta.expect("meta").version,
        "the replay is a replay, not a second write"
    );
}
