//! How an engine error becomes a status.
//!
//! Split out of `sql.rs` along the seam its module doc already named — scope
//! into a `WHERE` clause there, engine error into a status here — when the
//! title CHECK's mapping took that file over the line ceiling.

use tonic::Status;

/// An engine error is never returned to the caller verbatim: it carries table
/// names, column names and sometimes values. Logged here, generic on the wire.
///
/// EXCEPT for the one class that the caller can act on. A deadlock or a lock
/// wait timeout means "someone else held the row; try again", and flattening it
/// into `INTERNAL` tells a caller that a retryable wait is a permanent failure —
/// which is how a moment of contention becomes a user-visible error. `ABORTED`
/// is the code gRPC reserves for exactly this.
///
/// **AND A POOL THAT HAD NOTHING TO HAND OUT, which is the same argument one
/// layer further out and was missing.** `sqlx::Error::PoolTimedOut` is its own
/// variant rather than an `Error::Database`, so it never reached the arm above
/// and answered `INTERNAL` — a caller told that a stall lasting as long as the
/// acquire timeout is permanent, when every connection being busy is the most
/// transient condition this service has. It is reported `UNAVAILABLE` rather
/// than `ABORTED` because nothing was serialised against anything: the request
/// never reached the engine, and `UNAVAILABLE` is what a caller's backoff is
/// written for.
pub fn internal(e: sqlx::Error) -> Status {
    if matches!(e, sqlx::Error::PoolTimedOut) {
        tracing::warn!(error = %e, "task-db pool exhausted; the caller may retry");
        return Status::unavailable("no connection was free in time — retry");
    }
    if let sqlx::Error::Database(db) = &e {
        if let Some(status) = engine_status(&e, db.as_ref()) {
            return status;
        }
    }
    tracing::error!(error = %e, "task-db engine error");
    Status::internal("storage error")
}

/// The engine errors a caller can act on, and what each one answers; `None`
/// for every other one, which stays the opaque `INTERNAL`. A helper of its own
/// to keep `internal` under the cognitive-complexity ceiling once the title arm
/// joined it.
fn engine_status(e: &sqlx::Error, db: &dyn sqlx::error::DatabaseError) -> Option<Status> {
    if contended(db) {
        tracing::warn!(error = %e, "task-db lock contention; the caller may retry");
        return Some(Status::aborted("the write could not be serialised — retry"));
    }
    title_check_failed(db).then(|| {
        Status::invalid_argument("a task title is at most 128 characters (Unicode code points)")
    })
}

/// Whether an engine error is lock contention the caller may retry.
///
/// 1213 is ER_LOCK_DEADLOCK and 1205 ER_LOCK_WAIT_TIMEOUT. SQLSTATE 40001
/// covers the first; the second reports HY000 and is only identifiable by its
/// number.
fn contended(db: &dyn sqlx::error::DatabaseError) -> bool {
    db.code().as_deref() == Some("40001")
        || db
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|e| matches!(e.number(), 1205 | 1213))
}

/// Whether an engine error is migration 7's title CHECK failing.
///
/// MariaDB reports every failed CHECK as error 4025 with SQLSTATE 23000 — the
/// JSON validity checks on `tags` and `links` included — so the code alone
/// cannot say it was the title. The constraint's NAME is in the message, and
/// matching it is what keeps a fault that is not about a title from being
/// answered as one. Keyed on SQLSTATE rather than on the MySQL error number so
/// it can be tested without an engine.
fn title_check_failed(db: &dyn sqlx::error::DatabaseError) -> bool {
    db.code().as_deref() == Some("23000") && db.message().contains(crate::schema::TITLE_CHECK)
}

#[cfg(test)]
mod tests {
    use super::internal;

    /// A database error with a chosen SQLSTATE and message, for the mapping
    /// above. The engine's own error is proved in `tests/title_bound.rs`.
    #[derive(Debug)]
    struct Engine(&'static str, &'static str);
    impl std::fmt::Display for Engine {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.1)
        }
    }
    impl std::error::Error for Engine {}
    impl sqlx::error::DatabaseError for Engine {
        fn message(&self) -> &str {
            self.1
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::CheckViolation
        }
    }

    /// The title CHECK, as MariaDB words it, is the caller's mistake.
    #[test]
    fn the_title_check_failing_is_invalid_argument() {
        let status = internal(sqlx::Error::Database(Box::new(Engine(
            "23000",
            "CONSTRAINT `ck_task_title_len` failed for `td`.`task`",
        ))));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Any OTHER check failing — MariaDB names the JSON check after its column
    /// — stays the opaque INTERNAL: it is not about a title.
    #[test]
    fn a_different_check_failing_is_still_an_opaque_internal() {
        let status = internal(sqlx::Error::Database(Box::new(Engine(
            "23000",
            "CONSTRAINT `task.tags` failed for `td`.`task`",
        ))));
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(status.message(), "storage error");
    }

    /// **The one arm no engine test can reach once the allocator is fixed.**
    ///
    /// A pool timeout used to be producible on demand: `CreateTask` took a
    /// SECOND connection while holding its first, so at pool size N, N
    /// concurrent creates exhausted the pool and every one of them was answered
    /// `INTERNAL`. That path is gone and the mapping still has to be right — any
    /// later statement that waits on a busy pool must answer retryably. Built
    /// from the variant itself rather than provoked through an RPC, for the same
    /// reason `boot.rs` tests its decisions in-crate: the alternative is a test
    /// that can only assert what it can first break.
    #[test]
    fn a_pool_timeout_is_unavailable_and_therefore_retryable() {
        assert_eq!(
            internal(sqlx::Error::PoolTimedOut).code(),
            tonic::Code::Unavailable,
            "every connection being busy is the most transient condition this \
             service has, and must not be reported as a permanent failure"
        );
    }

    /// The default stays the default. Anything not named above is opaque and
    /// non-retryable, because an engine error carries table names, column names
    /// and sometimes values.
    #[test]
    fn any_other_engine_error_is_still_an_opaque_internal() {
        let status = internal(sqlx::Error::RowNotFound);
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(status.message(), "storage error");
    }
}
