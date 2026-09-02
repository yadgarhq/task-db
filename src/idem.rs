//! D9: a repeated idempotency key is a REPLAY, never a failure — for the
//! identical retry the rule was written for. As amended, D9 also settles the
//! case it used to leave open: a repeated key carrying a DIFFERENT payload is
//! refused with `INVALID_ARGUMENT` rather than replayed, because replaying it
//! would answer a request nobody made and report success.
//!
//! Behind a load balancer with retries a write fires more than once. The second
//! delivery must return the first one's outcome — not perform the write again,
//! and not report an error. The error is the worse of the two: under D39 it
//! tells an instance its write failed while the record exists, and the instance
//! "rectifies" that by writing a duplicate.
//!
//! The claim is taken INSIDE the caller's transaction, so a write and the record
//! that it happened commit together or not at all. A write that fails leaves no
//! claim, which makes its retry a fresh attempt rather than a replay of
//! something that never happened.
//!
//! The amendment needs a mechanism, because a uniqueness constraint detects a
//! repeat and cannot detect a DIFFERING repeat. `task_write` kept the prior
//! RESPONSE and never the prior request, so there was nothing here to compare a
//! new payload against. Migration 6 adds `request_fingerprint`, `claim` takes
//! the payload and binds its digest, and `replay` reads the column back and
//! compares. A NULL fingerprint — every row written before that migration —
//! replays, because an absent digest is not a differing one.
//!
//! **Which fields count as the payload is per-RPC**, and D9 requires that
//! stated per field rather than left to a reader. This module's answer, for all
//! three of its mutating RPCs: **every field of the request except `scope` and
//! `idempotency`.**
//!
//! - `CreateTask` — `task`.
//! - `UpdateTask` — `id`, `expect_version`, `task`, `update_mask`.
//! - `DeleteTask` — `id`, `expect_version`.
//!
//! `scope` is excluded because it is already the claim's key: a row is found by
//! `(project_id, user_id, idem_key)`, so a differing scope reaches a different
//! row and never a comparison. `idempotency` excludes itself — it carries the
//! key the comparison is keyed on. Nothing else is excluded, and the test for
//! whether a field belongs is D9's: whether a replay would silently discard the
//! difference. Every remaining field fails that test.
//!
//! `update_mask.paths` is SORTED before the digest is taken. `["title","body"]`
//! and `["body","title"]` name the same fields and encode to different bytes, so
//! an unsorted digest would refuse a request that discards nothing.

use prost::Message;
use sha2::{Digest, Sha256};
use sqlx::{MySql, Transaction};
use tonic::Status;

use crate::pb::yadgar::common::v1::{Idempotency, Scope};
use crate::sql::internal;

/// What to do about a key that has been offered.
pub enum Claimed<T> {
    /// No key, or a key never seen before: perform the write.
    Proceed,
    /// This key has been used. Return this and write nothing.
    Replay(T),
}

/// The key, once it is known to be worth recording. An absent or empty key means
/// the caller does not participate — which must not collapse every keyless write
/// into one deduplicated slot.
fn present(idem: Option<&Idempotency>) -> Option<&str> {
    idem.map(|i| i.key.as_str()).filter(|k| !k.is_empty())
}

/// The digest a claim is compared by. SHA-256, so it is exactly the 32 bytes
/// `request_fingerprint` holds.
pub fn fingerprint(payload: &[u8]) -> [u8; 32] {
    Sha256::digest(payload).into()
}

/// Take the key, or discover it is a replay.
///
/// `payload` is a thunk rather than bytes on purpose: a caller that supplies no
/// key does not participate, and encoding a request body it will never be
/// compared against is work nobody asked for.
pub async fn claim<T, P>(
    tx: &mut Transaction<'_, MySql>,
    scope: &Scope,
    rpc: &str,
    idem: Option<&Idempotency>,
    payload: P,
) -> Result<Claimed<T>, Status>
where
    T: Message + Default,
    P: FnOnce() -> Vec<u8>,
{
    let Some(key) = present(idem) else {
        return Ok(Claimed::Proceed);
    };
    let fingerprint = fingerprint(&payload());

    // Claim first and ask questions on collision. The alternative — read, then
    // insert if absent — has a window between the two in which the concurrent
    // retry this exists for lands. Here the second inserter BLOCKS on the unique
    // index until the first commits, then finds the committed outcome.
    //
    // The fingerprint is bound HERE rather than in `record`, so it commits with
    // the claim and inside the caller's transaction. A write that fails leaves
    // neither, which is what keeps its retry a fresh attempt.
    let claimed = sqlx::query(
        "INSERT INTO task_write
           (project_id, user_id, idem_key, rpc, response, request_fingerprint)
         VALUES (?, ?, ?, ?, '', ?)",
    )
    .bind(&scope.project_id)
    .bind(&scope.user_id)
    .bind(key)
    .bind(rpc)
    .bind(fingerprint.as_slice())
    .execute(&mut **tx)
    .await;

    match claimed {
        Ok(_) => Ok(Claimed::Proceed),
        Err(e) if is_duplicate(&e) => replay(tx, scope, rpc, key, &fingerprint).await,
        Err(e) => Err(internal(e)),
    }
}

async fn replay<T: Message + Default>(
    tx: &mut Transaction<'_, MySql>,
    scope: &Scope,
    rpc: &str,
    key: &str,
    fingerprint: &[u8; 32],
) -> Result<Claimed<T>, Status> {
    // `FOR UPDATE`, and it is not about locking. A plain SELECT here reads this
    // transaction's snapshot, which was taken before the row the other writer
    // just committed — so it would find nothing and the replay would look like a
    // fresh key. A locking read always sees the latest committed version.
    let row: (String, Vec<u8>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT rpc, response, request_fingerprint FROM task_write
          WHERE project_id = ? AND user_id = ? AND idem_key = ?
          FOR UPDATE",
    )
    .bind(&scope.project_id)
    .bind(&scope.user_id)
    .bind(key)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal)?;

    if row.0 != rpc {
        // Decoding a stored CreateTaskResponse as a DeleteTaskResponse would
        // succeed and mean nothing, so this is refused rather than guessed at.
        return Err(Status::invalid_argument(
            "this idempotency key was already used for a different operation",
        ));
    }

    // D9 as amended. A key reused with a DIFFERENT payload is refused, because
    // replaying it hands the first request's outcome to a caller who sent a
    // second: the operation actually asked for is discarded and the answer
    // reports success. The caller cannot tell, and refusing never lies.
    //
    // A NULL fingerprint is a claim recorded before migration 6 and is NOT a
    // difference — there is nothing to compare, so it replays as it always did.
    let recorded = row.2;
    if recorded.is_some_and(|r| r.as_slice() != fingerprint.as_slice()) {
        return Err(Status::invalid_argument(
            "this idempotency key was already used for a different request — \
             a replay would discard what this one asked for",
        ));
    }

    T::decode(row.1.as_slice())
        .map(Claimed::Replay)
        .map_err(|e| {
            tracing::error!(error = %e, rpc, "a recorded idempotent outcome no longer decodes");
            Status::internal("storage error")
        })
}

/// Record the outcome, so the retry that arrives in a moment has something to
/// return. Committed with the write itself.
pub async fn record<T: Message>(
    tx: &mut Transaction<'_, MySql>,
    scope: &Scope,
    idem: Option<&Idempotency>,
    response: &T,
) -> Result<(), Status> {
    let Some(key) = present(idem) else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE task_write SET response = ?
          WHERE project_id = ? AND user_id = ? AND idem_key = ?",
    )
    .bind(response.encode_to_vec())
    .bind(&scope.project_id)
    .bind(&scope.user_id)
    .bind(key)
    .execute(&mut **tx)
    .await
    .map_err(internal)?;
    Ok(())
}

fn is_duplicate(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    fn hex(digest: [u8; 32]) -> String {
        use std::fmt::Write as _;
        digest.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// KNOWN-ANSWER, and the answers come from OUTSIDE this crate.
    ///
    /// Every property this module is otherwise tested by survives a change of
    /// algorithm. "The same request fingerprints the same" holds for SHA-512
    /// truncated to 32 bytes, for BLAKE3, for anything deterministic — so the
    /// suite stays green while the FORMAT moves. What does not survive is the
    /// thirty-two bytes already sitting in `task_write.request_fingerprint`,
    /// written by the code as it was. Move the digest under those rows and
    /// `replay` compares a fresh digest against a stale one, finds them
    /// different, and refuses the identical retry D9 exists to replay — the core
    /// rule regressed by a change nothing failed on.
    ///
    /// So the bytes are pinned to literals, and the literals are PUBLISHED
    /// vectors rather than anything this code printed. Pasting back what the
    /// implementation emitted would assert the code against its own output and
    /// pass for any algorithm it happened to be using.
    ///
    /// FIPS 180-2 Appendix B.1 gives SHA-256("abc"); the empty-input digest is
    /// in NIST's CAVP set and in RFC 6234. Reproduce both without Rust:
    ///
    /// ```text
    /// printf 'abc' | sha256sum
    /// printf ''    | sha256sum
    /// ```
    #[test]
    fn a_fingerprint_is_sha256_and_the_bytes_are_the_published_vectors() {
        assert_eq!(
            hex(fingerprint(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "FIPS 180-2 Appendix B.1"
        );
        // Reachable rather than academic: a request whose every field is its
        // zero value encodes to no bytes at all, and that is the input this
        // function then receives.
        assert_eq!(
            hex(fingerprint(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "the empty-input vector"
        );
    }
}
