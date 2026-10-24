//! The pool acquire and the `BEGIN` after it, bounded by the caller and safe to
//! drop (ADR-1019).
//!
//! **THE ACQUIRE TAKES THE CALLER'S DEADLINE.** `database.acquireTimeoutSeconds`
//! is a CEILING, not the bound: [`acquire`] waits `min(ceiling, remaining -
//! DEADLINE_MARGIN)`, which is [`Deadline::bound`]. A caller with 300 ms left
//! gets `DEADLINE_EXCEEDED` from THIS module after 250 ms, recorded on its own
//! telemetry, instead of being dropped by tonic at 300 ms with nothing said.
//! No floor: a caller's deadline is never raised toward the ceiling.
//!
//! **THE `BEGIN` IS THE STEP A DROP CAN CORRUPT, AND SQLX DOES NOT COVER IT.**
//! Once callers send `grpc-timeout`, tonic's server drops a handler future at
//! the deadline wherever it is awaiting. sqlx 0.9 counts a transaction only
//! after `BEGIN`'s answer is read (`sqlx-mysql-0.9.0/src/transaction.rs`,
//! `transaction_depth += 1` after `execute`), and a `Transaction` dropped at
//! depth zero queues no `ROLLBACK` (`start_rollback` is a no-op there). The
//! connection then goes back to the pool, and the release `ping`
//! (`sqlx-core-0.9.0/src/pool/connection.rs`, `return_to_pool`) drains the
//! pending answer and rolls nothing back. The engine holds that connection in
//! a transaction sqlx believes is idle, and the next borrower's autocommit
//! read runs inside it, on one REPEATABLE READ snapshot for as long as the
//! connection lives. Measured against MariaDB 11.8 (`tests/deadline.rs`): the
//! next borrower sees `@@in_transaction = 1`.
//!
//! [`Held`] closes the connection instead of returning it when it is dropped
//! while a `BEGIN` it started has not answered. A drop at any OTHER point is
//! already safe: inside the transaction sqlx's own `Drop` queues `ROLLBACK`,
//! and outside one there is nothing to undo.
//!
//! **A GUARD, NOT A POOL HOOK.** `after_release` could run `ROLLBACK` on every
//! release, but the pool is built by `yadgar-store`'s `pool::connect`, so that
//! hook is a change to the shared crate. And sqlx's own `close_on_drop` cannot
//! be cleared once set, so the guard sets it only from its own `Drop`, and only
//! while armed.

use std::time::Duration;

use sqlx::pool::PoolConnection;
use sqlx::{MySql, MySqlConnection, MySqlPool, Transaction};
use tonic::Status;
use yadgar_dial::Deadline;

use crate::sql::internal;

/// One connection out of `pool`, waited for no longer than the caller can
/// still use it.
///
/// When the caller's deadline does not bind — none was sent, or more remains
/// than the ceiling — the acquire is sqlx's own, with its own timeout, and a
/// pool that stays full answers `UNAVAILABLE` exactly as before. Wrapping that
/// case too would race two equal timers and make the status a coin toss.
pub async fn acquire(pool: &MySqlPool, deadline: Deadline) -> Result<Held, Status> {
    let ceiling = pool.options().get_acquire_timeout();
    let bound = deadline.bound(ceiling);
    let conn = if bound >= ceiling {
        pool.acquire().await.map_err(internal)?
    } else if bound.is_zero() {
        return Err(spent(bound));
    } else {
        match tokio::time::timeout(bound, pool.acquire()).await {
            Ok(conn) => conn.map_err(internal)?,
            Err(_) => return Err(spent(bound)),
        }
    };
    Ok(Held { conn, armed: false })
}

fn spent(waited: Duration) -> Status {
    tracing::warn!(
        waited_ms = waited.as_millis() as u64,
        "task-db pool acquire outlived the caller's deadline"
    );
    Status::deadline_exceeded("the caller's deadline passed before a connection was free")
}

/// A pooled connection that is closed, not returned, if a `BEGIN` it started
/// never answered.
pub struct Held {
    conn: PoolConnection<MySql>,
    armed: bool,
}

impl Held {
    /// `BEGIN` on this connection. Armed for exactly the span in which a drop
    /// would leave the engine inside a transaction sqlx does not know about.
    ///
    /// Left armed on an error too: a `BEGIN` that failed says nothing certain
    /// about the session, and a closed connection costs one reconnect.
    pub async fn begin(&mut self) -> Result<Transaction<'_, MySql>, Status> {
        self.armed = true;
        let tx = sqlx::Acquire::begin(&mut self.conn)
            .await
            .map_err(internal)?;
        self.armed = false;
        Ok(tx)
    }

    /// The connection, for a statement outside any transaction.
    pub fn conn(&mut self) -> &mut MySqlConnection {
        &mut self.conn
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!("task-db closed a connection whose BEGIN never answered");
            self.conn.close_on_drop();
        }
    }
}
