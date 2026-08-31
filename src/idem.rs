//! D9: a repeated idempotency key is a REPLAY, never a failure.
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

use prost::Message;
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

/// Take the key, or discover it is a replay.
pub async fn claim<T: Message + Default>(
    tx: &mut Transaction<'_, MySql>,
    scope: &Scope,
    rpc: &str,
    idem: Option<&Idempotency>,
) -> Result<Claimed<T>, Status> {
    let Some(key) = present(idem) else {
        return Ok(Claimed::Proceed);
    };

    // Claim first and ask questions on collision. The alternative — read, then
    // insert if absent — has a window between the two in which the concurrent
    // retry this exists for lands. Here the second inserter BLOCKS on the unique
    // index until the first commits, then finds the committed outcome.
    let claimed = sqlx::query(
        "INSERT INTO task_write (project_id, user_id, idem_key, rpc, response)
         VALUES (?, ?, ?, ?, '')",
    )
    .bind(&scope.project_id)
    .bind(&scope.user_id)
    .bind(key)
    .bind(rpc)
    .execute(&mut **tx)
    .await;

    match claimed {
        Ok(_) => Ok(Claimed::Proceed),
        Err(e) if is_duplicate(&e) => replay(tx, scope, rpc, key).await,
        Err(e) => Err(internal(e)),
    }
}

async fn replay<T: Message + Default>(
    tx: &mut Transaction<'_, MySql>,
    scope: &Scope,
    rpc: &str,
    key: &str,
) -> Result<Claimed<T>, Status> {
    // `FOR UPDATE`, and it is not about locking. A plain SELECT here reads this
    // transaction's snapshot, which was taken before the row the other writer
    // just committed — so it would find nothing and the replay would look like a
    // fresh key. A locking read always sees the latest committed version.
    let row: (String, Vec<u8>) = sqlx::query_as(
        "SELECT rpc, response FROM task_write
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
