//! KNOWN-ANSWER TESTS for the one value this module writes to storage as
//! opaque bytes: `task_write.request_fingerprint`.
//!
//! **Why these exist beside the D9 suite rather than inside it.** Every other
//! test of the fingerprint is a PROPERTY: the same request replays, a different
//! one is refused, a reordered mask is the same request. All three survive a
//! change to the algorithm, to the encoding, or to which fields are cleared
//! before the digest is taken — hash both sides the new way and each property
//! still holds. What does not survive is the thirty-two bytes ALREADY IN THE
//! TABLE, which were written by the code as it was on the day. `replay` then
//! compares a fresh digest against a stale one, finds them different, and
//! refuses the identical retry D9 exists to replay. The suite stays green and
//! every claim already recorded becomes a refusal.
//!
//! So each test below pins the exact bytes for a fixed request, END TO END:
//! through `Payload`'s canonicalisation, through SHA-256, and through the
//! `BINARY(32)` column, read back from MariaDB rather than from the function.
//! That last leg is not decoration — it is what pins the STORAGE encoding. A
//! change to raw-versus-hex, or a digest of some other width padded out by
//! `BINARY(32)`, is invisible to any assertion that stops at the return value.
//!
//! **THE EXPECTED VALUES ARE DERIVED, NEVER PASTED.** Running the code and
//! recording what it printed would assert the implementation against its own
//! output, which passes for whatever the implementation currently does — the
//! failure this whole class of test exists to stop. Each digest below is instead
//! computed from bytes written out by hand from the protobuf wire format, and
//! reproduced with coreutils. Every test carries the `printf | sha256sum` that
//! regenerates it without Rust.
//!
//! Reading a wire-format literal: a field is a varint key
//! `(field_number << 3) | wire_type`, then its payload. Wire type 0 is a varint,
//! wire type 2 is a length then that many bytes. Field numbers come from
//! `proto/yadgar/task/v1/task.proto`, and prost emits fields in ascending field
//! number. **`Task`'s content fields are numbered 16 and above, so their keys
//! are TWO-byte varints** — `title` is field 17, key `17<<3|2 = 138`, which
//! encodes as `8a 01` and not as one byte. That is the step a hand-derivation
//! most easily gets wrong, so it is spelled out rather than trusted.

mod support;

use support::{World, P_A, U1};
use tonic::Request;
use yadgar_task_db::pb::yadgar::common::v1::{Idempotency, Visibility};
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

fn key(k: &str) -> Option<Idempotency> {
    Some(Idempotency { key: k.into() })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The id every seeded row below carries, and the reason they are seeded at all.
///
/// `CreateTask` mints a UUIDv7, so a request built around a created task has a
/// different `id` on every run and cannot be pinned to a literal. `seed_row`
/// composes its id from the project and the number, so it is the same string on
/// every run — which is what makes `UpdateTask` and `DeleteTask` reachable by a
/// known-answer test at all.
const SEEDED_ID: &str = "yadgar:task:pre-acme/a-00001";

/// `CreateTask`: the nested-message case, and the one that pins the exclusions.
///
/// The request carries a `scope` and an `idempotency`, and `Payload` clears both
/// before the digest — so if either ever stopped being cleared, the stored bytes
/// would change and this test would say so. Nothing else would: a scope is
/// constant within a test, so every property here keeps holding.
///
/// The canonical payload, written out from the wire format:
///
/// ```text
///   1a 09                    field 3 (task), wire type 2, 9 bytes follow
///      8a 01 03 6b 61 74     field 17 (title), type 2, 3 bytes, "kat"
///      98 01 01              field 19 (status), type 0, 1 = TASK_STATUS_OPEN
/// ```
///
/// Both inner keys are two-byte varints, `title` being 17<<3|2 = 138 and
/// `status` 19<<3|0 = 152. `scope` (field 2) and `idempotency` (field 1) are
/// absent, which is the point.
///
/// ```text
/// printf '\x1a\x09\x8a\x01\x03\x6b\x61\x74\x98\x01\x01' | sha256sum
/// ```
#[tokio::test]
async fn the_stored_fingerprint_of_a_create_is_exactly_these_bytes() {
    let w = World::fresh("td_kat_create").await;

    w.db.create_task(Request::new(CreateTaskRequest {
        scope: w.scope(P_A, U1),
        task: Some(Task {
            title: "kat".into(),
            status: TaskStatus::Open as i32,
            ..Default::default()
        }),
        idempotency: key("kat-create"),
    }))
    .await
    .expect("create");

    let stored = w
        .stored_fingerprint(P_A, U1, "kat-create")
        .await
        .expect("a claim taken with a key must record a fingerprint");

    assert_eq!(
        hex(&stored),
        "42dbf806cf003da1bdf0e1c56ed309a85e523b31c86c85a181ee510b0070a550",
        "the bytes written for a CreateTask claim have moved"
    );
    assert_eq!(
        stored.len(),
        32,
        "BINARY(32) pads a short value rather than refusing it, so the width is \
         pinned here or not at all"
    );
}

/// `UpdateTask`: the case that pins the mask NORMALISATION, which is the part of
/// the framing a reader is most likely to reach for.
///
/// The request names `["title", "body", "title"]`. `Payload` sorts and dedups,
/// so the digest is taken over `["body", "title"]` — and the literal below is
/// the sorted, deduplicated form. Delete either the `sort` or the `dedup` and
/// the stored bytes change.
///
/// The canonical payload:
///
/// ```text
///   1a 1c <28 bytes>         field 3 (id), type 2, "yadgar:task:pre-acme/a-00001"
///   20 01                    field 4 (expect_version), type 0, 1
///   2a 06                    field 5 (task), type 2, 6 bytes follow
///      8a 01 03 6b 61 74     field 17 (title), type 2, 3 bytes, "kat"
///   32 0d                    field 6 (update_mask), type 2, 13 bytes follow
///      0a 04 62 6f 64 79     FieldMask field 1 (paths), "body"
///      0a 05 74 69 74 6c 65  FieldMask field 1 (paths), "title"
/// ```
///
/// `status` is absent from the inner `Task` because proto3 omits a field at its
/// zero value, and `TASK_STATUS_UNSPECIFIED` is 0.
///
/// ```text
/// printf '\x1a\x1c\x79\x61\x64\x67\x61\x72\x3a\x74\x61\x73\x6b\x3a\x70\x72\x65\x2d\x61\x63\x6d\x65\x2f\x61\x2d\x30\x30\x30\x30\x31\x20\x01\x2a\x06\x8a\x01\x03\x6b\x61\x74\x32\x0d\x0a\x04\x62\x6f\x64\x79\x0a\x05\x74\x69\x74\x6c\x65' | sha256sum
/// ```
#[tokio::test]
async fn the_stored_fingerprint_of_an_update_is_exactly_these_bytes() {
    let w = World::fresh("td_kat_update").await;
    let id = w.seed_row(P_A, U1, 1, Visibility::Private as i8, "").await;
    assert_eq!(id, SEEDED_ID, "the literal below is built from this id");

    w.db.update_task(Request::new(UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id,
        expect_version: 1,
        task: Some(Task {
            title: "kat".into(),
            ..Default::default()
        }),
        // Deliberately unsorted AND carrying a duplicate. A mask is a set of
        // field names, so this asks for precisely what the sorted form asks
        // for — which is why the digest is taken over the sorted form.
        update_mask: Some(prost_types::FieldMask {
            paths: vec!["title".into(), "body".into(), "title".into()],
        }),
        idempotency: key("kat-update"),
    }))
    .await
    .expect("update");

    assert_eq!(
        hex(&w
            .stored_fingerprint(P_A, U1, "kat-update")
            .await
            .expect("a claim taken with a key must record a fingerprint")),
        "bb36ca9e753a7a6eaf484c6ff4953d2d04906ccbfaea07b717c8bf1864279c7a",
        "the bytes written for an UpdateTask claim have moved — check the mask \
         sort and dedup before anything else"
    );
}

/// `DeleteTask`: the flat case, two scalar fields and nothing nested.
///
/// The canonical payload:
///
/// ```text
///   1a 1c <28 bytes>   field 3 (id), type 2, "yadgar:task:pre-acme/a-00001"
///   20 01              field 4 (expect_version), type 0, 1
/// ```
///
/// ```text
/// printf '\x1a\x1c\x79\x61\x64\x67\x61\x72\x3a\x74\x61\x73\x6b\x3a\x70\x72\x65\x2d\x61\x63\x6d\x65\x2f\x61\x2d\x30\x30\x30\x30\x31\x20\x01' | sha256sum
/// ```
#[tokio::test]
async fn the_stored_fingerprint_of_a_delete_is_exactly_these_bytes() {
    let w = World::fresh("td_kat_delete").await;
    let id = w.seed_row(P_A, U1, 1, Visibility::Private as i8, "").await;
    assert_eq!(id, SEEDED_ID, "the literal below is built from this id");

    w.db.delete_task(Request::new(DeleteTaskRequest {
        scope: w.scope(P_A, U1),
        id,
        expect_version: 1,
        idempotency: key("kat-delete"),
    }))
    .await
    .expect("delete");

    assert_eq!(
        hex(&w
            .stored_fingerprint(P_A, U1, "kat-delete")
            .await
            .expect("a claim taken with a key must record a fingerprint")),
        "59fe72d80c2dfe369eea83169ec74abce0f14476a4c86298f678e612cf01c758",
        "the bytes written for a DeleteTask claim have moved"
    );
}
