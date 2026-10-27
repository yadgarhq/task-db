//! `caller_fingerprint` on `UpdateTask` and `DeleteTask` (ADR-1051 rule 4), and
//! migration 8, which gives the claim a column to keep it in.
//!
//! **WHICH COMPARISON DECIDES A CLAIM COLLISION.** When BOTH the stored claim
//! and the retry carry a caller fingerprint, that comparison decides, and the
//! store's own request fingerprint is not consulted. Otherwise the store
//! fingerprint decides, exactly as before the field existed. The tests are
//! built so that the two comparisons DISAGREE in every arm — a store payload
//! that differs under equal caller fingerprints, an identical one under
//! different caller fingerprints — because a test where both agree passes for
//! an implementation that ignores one of them.
//!
//! Empty means absent and stores NULL. Any other length than 32 is refused
//! before any database access.

mod support;

use prost::Message as _;
use support::{World, P_A, U1};
use tonic::{Code, Request, Status};
use yadgar_task_db::pb::yadgar::common::v1::{Idempotency, Meta};
use yadgar_task_db::pb::yadgar::task::v1::lookup_write_response::Outcome;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

const DIFFERENT_REQUEST: &str = "already used for a different request";

fn key(k: &str) -> Option<Idempotency> {
    Some(Idempotency { key: k.into() })
}

fn fp(n: u8) -> Vec<u8> {
    vec![n; 32]
}

fn edit(w: &World, id: &str, title: &str, k: &str, caller: Vec<u8>) -> UpdateTaskRequest {
    UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.into(),
        expect_version: 1,
        task: Some(Task {
            title: title.into(),
            body: "b".into(),
            status: TaskStatus::Open as i32,
            ..Default::default()
        }),
        update_mask: None,
        caller_fingerprint: caller,
        idempotency: key(k),
    }
}

async fn update(w: &World, req: UpdateTaskRequest) -> Result<UpdateTaskResponse, Status> {
    w.db.update_task(Request::new(req))
        .await
        .map(|r| r.into_inner())
}

fn remove(w: &World, id: &str, version: u64, k: &str, caller: Vec<u8>) -> DeleteTaskRequest {
    DeleteTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.into(),
        expect_version: version,
        caller_fingerprint: caller,
        idempotency: key(k),
    }
}

async fn delete(w: &World, req: DeleteTaskRequest) -> Result<DeleteTaskResponse, Status> {
    w.db.delete_task(Request::new(req))
        .await
        .map(|r| r.into_inner())
}

fn different_request(err: Status) {
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(err.message().contains(DIFFERENT_REQUEST), "{err:?}");
}

/// EQUAL CALLER FINGERPRINTS REPLAY, EVEN OVER A DIFFERENT STORE PAYLOAD.
/// This is ADR-1051's case 2: task copies fields out of its read, so a retry
/// after a concurrent change sends a different store payload for the SAME
/// caller request. The store fingerprint alone would refuse it.
#[tokio::test]
async fn equal_caller_fingerprints_replay_over_a_different_store_payload() {
    let w = World::fresh("td_cfp_equal").await;
    let id = w.create(P_A, U1, "before").await;
    let first = update(&w, edit(&w, &id, "first", "k", fp(1)))
        .await
        .expect("first");

    let replay = update(&w, edit(&w, &id, "what a re-read produced", "k", fp(1)))
        .await
        .expect("equal caller fingerprints decide, and they replay");
    assert_eq!(replay, first);
    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").title,
        "first",
        "a replay writes nothing"
    );
}

/// DIFFERENT CALLER FINGERPRINTS ARE REFUSED, EVEN OVER AN IDENTICAL STORE
/// PAYLOAD. The store fingerprint alone would replay this.
#[tokio::test]
async fn different_caller_fingerprints_are_refused_over_an_identical_store_payload() {
    let w = World::fresh("td_cfp_differ").await;
    let id = w.create(P_A, U1, "before").await;
    update(&w, edit(&w, &id, "same", "k", fp(1)))
        .await
        .expect("first");

    different_request(
        update(&w, edit(&w, &id, "same", "k", fp(2)))
            .await
            .expect_err("different caller fingerprints decide, and they refuse"),
    );
}

/// THE STORED CLAIM CARRIES NONE: the store fingerprint decides.
#[tokio::test]
async fn a_claim_without_a_caller_fingerprint_falls_back_to_the_store_fingerprint() {
    let w = World::fresh("td_cfp_row_absent").await;
    let id = w.create(P_A, U1, "before").await;
    let first = update(&w, edit(&w, &id, "same", "k", vec![]))
        .await
        .expect("first, without a caller fingerprint");

    assert_eq!(
        update(&w, edit(&w, &id, "same", "k", fp(1)))
            .await
            .expect("same store payload replays"),
        first
    );
    different_request(
        update(&w, edit(&w, &id, "different", "k", fp(1)))
            .await
            .expect_err("a different store payload is refused"),
    );
}

/// THE RETRY CARRIES NONE: the store fingerprint decides.
#[tokio::test]
async fn a_retry_without_a_caller_fingerprint_falls_back_to_the_store_fingerprint() {
    let w = World::fresh("td_cfp_request_absent").await;
    let id = w.create(P_A, U1, "before").await;
    let first = update(&w, edit(&w, &id, "same", "k", fp(1)))
        .await
        .expect("first, with a caller fingerprint");

    assert_eq!(
        update(&w, edit(&w, &id, "same", "k", vec![]))
            .await
            .expect("same store payload replays"),
        first
    );
    different_request(
        update(&w, edit(&w, &id, "different", "k", vec![]))
            .await
            .expect_err("a different store payload is refused"),
    );
}

/// The same rules on `DeleteTask`, whose store payload is `id` and
/// `expect_version`.
#[tokio::test]
async fn a_delete_collision_is_decided_by_the_caller_fingerprints_when_both_carry_one() {
    let w = World::fresh("td_cfp_delete").await;
    let id = w.create(P_A, U1, "doomed").await;
    delete(&w, remove(&w, &id, 1, "k", fp(1)))
        .await
        .expect("first");

    delete(&w, remove(&w, &id, 7, "k", fp(1)))
        .await
        .expect("equal caller fingerprints replay over a different expect_version");
    different_request(
        delete(&w, remove(&w, &id, 1, "k", fp(2)))
            .await
            .expect_err("different caller fingerprints refuse an identical payload"),
    );
    delete(&w, remove(&w, &id, 1, "k", vec![]))
        .await
        .expect("one side absent: the identical store payload replays");
    different_request(
        delete(&w, remove(&w, &id, 7, "k", vec![]))
            .await
            .expect_err("one side absent: a different store payload is refused"),
    );
}

/// What the claim stores: the 32 bytes as sent, or NULL for an empty field —
/// never the 32 zero bytes `BINARY(32)` would pad an empty value out to.
#[tokio::test]
async fn the_claim_stores_the_caller_fingerprint_and_null_when_it_is_empty() {
    let w = World::fresh("td_cfp_stored").await;
    let id = w.create(P_A, U1, "before").await;
    let other = w.create(P_A, U1, "other").await;
    let sent: Vec<u8> = (0u8..32).collect();

    update(&w, edit(&w, &id, "after", "k-with", sent.clone()))
        .await
        .expect("with");
    update(&w, edit(&w, &other, "after", "k-without", vec![]))
        .await
        .expect("without");
    delete(&w, remove(&w, &id, 2, "k-delete", fp(4)))
        .await
        .expect("delete");

    assert_eq!(
        w.stored_caller_fingerprint(P_A, U1, "k-with").await,
        Some(sent)
    );
    assert_eq!(
        w.stored_caller_fingerprint(P_A, U1, "k-without").await,
        None
    );
    assert_eq!(
        w.stored_caller_fingerprint(P_A, U1, "k-delete").await,
        Some(fp(4))
    );
}

/// Any length other than 0 or 32 is refused with INVALID_ARGUMENT, on a closed
/// pool so the refusal is proved to come before any database access.
#[tokio::test]
async fn a_caller_fingerprint_of_the_wrong_length_is_refused_before_any_database_access() {
    let w = World::fresh("td_cfp_length").await;
    let id = w.create(P_A, U1, "before").await;
    w.pool.close().await;

    for len in [1usize, 16, 31, 33, 64] {
        let err = update(&w, edit(&w, &id, "after", "k", vec![7; len]))
            .await
            .expect_err("update");
        assert_eq!(err.code(), Code::InvalidArgument, "update, {len}: {err:?}");
        let err = delete(&w, remove(&w, &id, 1, "k", vec![7; len]))
            .await
            .expect_err("delete");
        assert_eq!(err.code(), Code::InvalidArgument, "delete, {len}: {err:?}");
    }

    // The control: 0 and 32 pass the check and then fail at the closed pool.
    for caller in [vec![], fp(1)] {
        let err = update(&w, edit(&w, &id, "after", "k", caller.clone()))
            .await
            .expect_err("a closed pool");
        assert_ne!(err.code(), Code::InvalidArgument, "update: {err:?}");
        let err = delete(&w, remove(&w, &id, 1, "k", caller))
            .await
            .expect_err("a closed pool");
        assert_ne!(err.code(), Code::InvalidArgument, "delete: {err:?}");
    }
}

/// Migration 8 on a store at 7 that already holds claims: the new column is
/// NULL on every existing row, nothing else moves, and those claims replay
/// through both the lookup and a colliding write.
///
/// Seeded with raw SQL because today's `claim` binds the column migration 8
/// adds, so the service cannot write the BEFORE state.
#[tokio::test]
async fn migration_8_adds_a_null_caller_fingerprint_to_every_existing_claim() {
    let w = World::fresh_at("td_cfp_mig8", 7).await;
    let id = w.create(P_A, U1, "before").await;

    let recorded = UpdateTaskResponse {
        meta: Some(Meta {
            id: id.clone(),
            version: 2,
            project_id: P_A.into(),
            ..Default::default()
        }),
        previous_status: TaskStatus::Open as i32,
    };
    let stored_request = vec![0xab_u8; 32];
    for (k, request_fingerprint) in [("k-old", Some(stored_request.clone())), ("k-older", None)] {
        sqlx::query(
            "INSERT INTO task_write
               (project_id, user_id, idem_key, rpc, response, request_fingerprint)
             VALUES (?, ?, ?, 'UpdateTask', ?, ?)",
        )
        .bind(P_A)
        .bind(U1)
        .bind(k)
        .bind(recorded.encode_to_vec())
        .bind(request_fingerprint)
        .execute(&w.pool)
        .await
        .expect("seed a claim the build at migration 7 wrote");
    }

    assert_eq!(w.migrate_to_head().await, 8);

    for k in ["k-old", "k-older"] {
        assert_eq!(w.stored_caller_fingerprint(P_A, U1, k).await, None, "{k}");
    }
    assert_eq!(
        w.stored_fingerprint(P_A, U1, "k-old").await,
        Some(stored_request),
        "the request fingerprint is untouched"
    );
    assert_eq!(w.stored_fingerprint(P_A, U1, "k-older").await, None);

    let found =
        w.db.lookup_write(Request::new(LookupWriteRequest {
            scope: w.scope(P_A, U1),
            idempotency: key("k-old"),
            rpc: WriteRpc::UpdateTask as i32,
            caller_fingerprint: fp(1),
        }))
        .await
        .expect("lookup")
        .into_inner();
    assert_eq!(found.outcome, Some(Outcome::Update(recorded.clone())));

    // A colliding write with a caller fingerprint against a NULL row falls back
    // to the store fingerprint, which is NULL on `k-older` and so replays.
    assert_eq!(
        update(&w, edit(&w, &id, "anything", "k-older", fp(1)))
            .await
            .expect("a NULL row replays"),
        recorded
    );
}
