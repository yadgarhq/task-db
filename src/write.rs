//! `CreateTask`, `UpdateTask` and `DeleteTask`.
//!
//! One RPC is one transaction (D5), and the D9 claim is taken inside it, so a
//! write and the record that it happened commit together or not at all.

use prost_types::FieldMask;
use sqlx::mysql::MySqlArguments;
use sqlx::query::Query;
use sqlx::types::Json;
use sqlx::{MySql, Transaction};
use tonic::Status;

use crate::idem::{self, Claimed};
use crate::pb::yadgar::common::v1::{Meta, Scope, Visibility};
use crate::pb::yadgar::task::v1::*;
use crate::service::TaskDb;
use crate::sql::{internal, scope_of, Reach};

impl TaskDb {
    pub(crate) async fn create(
        &self,
        req: CreateTaskRequest,
    ) -> Result<CreateTaskResponse, Status> {
        let scope = scope_of(&req.scope)?;
        let task = req
            .task
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("task is required"))?;
        assigned_by_the_module(task.meta.as_ref())?;

        let mut tx = self.pool.begin().await.map_err(internal)?;
        if let Claimed::Replay(original) =
            idem::claim(&mut tx, scope, "CreateTask", req.idempotency.as_ref()).await?
        {
            tx.rollback().await.map_err(internal)?;
            return Ok(original);
        }

        // UUIDv7: time-ordered, so keyset pagination and index locality behave
        // (D42). The URN is what leaves this service; the raw uuid never does.
        let id = format!("yadgar:task:{}", uuid::Uuid::now_v7());
        let number = self.allocate_number(&mut tx, &scope.project_id).await?;
        insert(&mut tx, &id, scope, number, task).await?;

        let response = CreateTaskResponse {
            meta: Some(Meta {
                id,
                version: 1,
                project_id: scope.project_id.clone(),
                owner_user_id: scope.user_id.clone(),
                ..Default::default()
            }),
            number,
        };
        idem::record(&mut tx, scope, req.idempotency.as_ref(), &response).await?;
        tx.commit().await.map_err(internal)?;
        Ok(response)
    }

    /// The next number for a project, allocated under a lock that actually
    /// exists.
    ///
    /// The counter ROW is created FIRST, in its own autocommitted statement,
    /// before this transaction takes any lock on that table.
    ///
    /// Reading first and creating the row only when the read comes back empty is
    /// the obvious shape and it self-deadlocks: a `SELECT ... FOR UPDATE` that
    /// matches nothing leaves a GAP lock behind, and the insert that would fill
    /// the gap then waits on the very transaction that is about to perform it.
    /// Measured, not reasoned about — every create hung for the full lock-wait
    /// timeout.
    ///
    /// Two callers racing on a brand-new project are safe here because each
    /// `INSERT IGNORE` is its own transaction: the second waits for the first to
    /// commit, finds the row, and ignores its own insert. Two transactions
    /// inserting the same new key inside their own transactions would instead
    /// deadlock on each other's insert-intent locks.
    async fn allocate_number(
        &self,
        tx: &mut Transaction<'_, MySql>,
        project: &str,
    ) -> Result<u32, Status> {
        sqlx::query("INSERT IGNORE INTO task_counter (project_id, next_number) VALUES (?, 0)")
            .bind(project)
            .execute(&self.pool)
            .await
            .map_err(internal)?;

        // A locking read sees the latest committed row rather than this
        // transaction's snapshot, so the second allocator reads what the first
        // one wrote instead of the value it started with. THIS is the lock the
        // old `SELECT MAX(number) + 1 ... FOR UPDATE` failed to take: there were
        // no rows to lock in an empty project, so it took nothing at all.
        let last: u32 = sqlx::query_scalar(
            "SELECT next_number FROM task_counter WHERE project_id = ? FOR UPDATE",
        )
        .bind(project)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?;

        let number = last + 1;
        sqlx::query("UPDATE task_counter SET next_number = ? WHERE project_id = ?")
            .bind(number)
            .bind(project)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
        Ok(number)
    }

    pub(crate) async fn update(
        &self,
        req: UpdateTaskRequest,
    ) -> Result<UpdateTaskResponse, Status> {
        let scope = scope_of(&req.scope)?;
        let task = req
            .task
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("task is required"))?;
        let fields = fields_of(req.update_mask.as_ref())?;
        let reach = Reach::of(scope);

        let mut tx = self.pool.begin().await.map_err(internal)?;
        if let Claimed::Replay(original) =
            idem::claim(&mut tx, scope, "UpdateTask", req.idempotency.as_ref()).await?
        {
            tx.rollback().await.map_err(internal)?;
            return Ok(original);
        }

        // Compare-and-set (D8): the version is in the WHERE, so a concurrent
        // writer's update makes this one match zero rows rather than silently
        // overwriting. The D12 ladder is in the same WHERE, so a record the
        // caller may not see is also one it cannot edit — UpdateTask had no
        // owner check at all while DeleteTask did.
        let sets = fields
            .iter()
            .map(|f| format!("{} = ?", f.column()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE task SET {sets}, version = version + 1, updated_by = ?
              WHERE id = ? AND version = ? AND deleted_at IS NULL AND {}",
            reach.predicate()
        );

        // AUDIT: the interpolations are this module's own column names and
        // predicate; every caller value is a bound parameter.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for field in &fields {
            query = field.bind(query, task);
        }
        let query = query
            .bind(&scope.user_id)
            .bind(&req.id)
            .bind(req.expect_version);
        let result = reach
            .bind(query)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;

        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(internal)?;
            return Err(Status::failed_precondition(
                "version mismatch, or no such task in this scope — re-read and retry",
            ));
        }

        let response = UpdateTaskResponse {
            meta: Some(Meta {
                id: req.id.clone(),
                version: req.expect_version + 1,
                project_id: scope.project_id.clone(),
                ..Default::default()
            }),
        };
        idem::record(&mut tx, scope, req.idempotency.as_ref(), &response).await?;
        tx.commit().await.map_err(internal)?;
        Ok(response)
    }

    pub(crate) async fn delete(
        &self,
        req: DeleteTaskRequest,
    ) -> Result<DeleteTaskResponse, Status> {
        let scope = scope_of(&req.scope)?;
        let reach = Reach::of(scope);

        let mut tx = self.pool.begin().await.map_err(internal)?;
        if let Claimed::Replay(original) =
            idem::claim(&mut tx, scope, "DeleteTask", req.idempotency.as_ref()).await?
        {
            tx.rollback().await.map_err(internal)?;
            return Ok(original);
        }

        // Soft, and OWNER-ONLY (D26). The owner check is in the statement rather
        // than a read-then-decide, so it cannot race with a change of owner —
        // and it is narrower than the D12 ladder, which is why the ladder is not
        // ANDed on top: an owner must not lose their own record by leaving the
        // team it was shared with.
        let sql = format!(
            "UPDATE task SET deleted_at = CURRENT_TIMESTAMP, version = version + 1
              WHERE id = ? AND version = ? AND deleted_at IS NULL
                AND owner_user_id = ? AND {}",
            reach.within()
        );
        // AUDIT: as above.
        let result = reach
            .bind_within(
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(&req.id)
                    .bind(req.expect_version)
                    .bind(&scope.user_id),
            )
            .execute(&mut *tx)
            .await
            .map_err(internal)?;

        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(internal)?;
            return Err(Status::failed_precondition(
                "version mismatch, not the owner, or no such task in this scope",
            ));
        }

        let response = DeleteTaskResponse {};
        idem::record(&mut tx, scope, req.idempotency.as_ref(), &response).await?;
        tx.commit().await.map_err(internal)?;
        Ok(response)
    }
}

async fn insert(
    tx: &mut Transaction<'_, MySql>,
    id: &str,
    scope: &Scope,
    number: u32,
    task: &Task,
) -> Result<(), Status> {
    sqlx::query(
        "INSERT INTO task
           (id, project_id, owner_user_id, team_id, visibility,
            created_by, updated_by, number, title, body, status, tags, links)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(&scope.project_id)
    .bind(&scope.user_id)
    // team_id and visibility are ASSIGNED, never taken from the request (D42),
    // and D12's ladder defaults to its most restrictive rung. A record is shared
    // by a later, deliberate act — never by the same call that creates it.
    .bind("")
    .bind(Visibility::Private as i8)
    .bind(&scope.user_id)
    .bind(&scope.user_id)
    .bind(number)
    .bind(&task.title)
    .bind(&task.body)
    .bind(task.status as i8)
    .bind(Json(&task.tags))
    .bind(Json(&task.links))
    .execute(&mut **tx)
    .await
    .map_err(internal)?;
    Ok(())
}

/// D42: the caller supplies CONTENT, never identity.
///
/// `team_id` and `visibility` used to be bound straight from the request, so a
/// caller could publish its own record to the whole organisation — and the
/// `unwrap_or(1)` default meant an unset field persisted
/// `VISIBILITY_UNSPECIFIED`, which common.proto says is never stored.
///
/// Refused rather than silently overwritten: a caller that asked for ORG and
/// received PRIVATE without being told believes the record is shared.
fn assigned_by_the_module(meta: Option<&Meta>) -> Result<(), Status> {
    let Some(meta) = meta else { return Ok(()) };
    let supplied = !meta.id.is_empty()
        || meta.version != 0
        || !meta.project_id.is_empty()
        || !meta.owner_user_id.is_empty()
        || !meta.team_id.is_empty()
        || meta.visibility != 0
        || !meta.created_by.is_empty()
        || !meta.updated_by.is_empty()
        || !meta.derived_from.is_empty();
    if supplied {
        return Err(Status::invalid_argument(
            "meta is assigned by this module and must be empty on create (D42)",
        ));
    }
    Ok(())
}

/// The columns an update may write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Body,
    Status,
    Tags,
    Links,
}

impl Field {
    const ALL: [Field; 5] = [
        Field::Title,
        Field::Body,
        Field::Status,
        Field::Tags,
        Field::Links,
    ];

    fn column(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Body => "body",
            Field::Status => "status",
            Field::Tags => "tags",
            Field::Links => "links",
        }
    }

    fn named(path: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.column() == path)
    }

    fn bind<'q>(
        self,
        query: Query<'q, MySql, MySqlArguments>,
        task: &'q Task,
    ) -> Query<'q, MySql, MySqlArguments> {
        match self {
            Field::Title => query.bind(&task.title),
            Field::Body => query.bind(&task.body),
            Field::Status => query.bind(task.status as i8),
            Field::Tags => query.bind(Json(&task.tags)),
            Field::Links => query.bind(Json(&task.links)),
        }
    }
}

/// An absent or empty mask means every field, which is what this service did
/// before it read the mask at all — so an older caller keeps its behaviour.
///
/// A mask that NAMES fields is honoured, and that is what lets `EditTask` write
/// a title without also writing a status it had to read first.
fn fields_of(mask: Option<&FieldMask>) -> Result<Vec<Field>, Status> {
    let Some(mask) = mask.filter(|m| !m.paths.is_empty()) else {
        return Ok(Field::ALL.to_vec());
    };
    mask.paths
        .iter()
        .map(|path| {
            Field::named(path).ok_or_else(|| {
                Status::invalid_argument(format!("update_mask names an unknown field: {path}"))
            })
        })
        .collect()
}
