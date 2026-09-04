//! `CreateTask`, `UpdateTask` and `DeleteTask`.
//!
//! One RPC is one transaction (D5), and the D9 claim is taken inside it, so a
//! write and the record that it happened commit together or not at all.

use prost::Message as _;
use prost_types::FieldMask;
use sqlx::mysql::MySqlArguments;
use sqlx::query::Query;
use sqlx::types::Json;
use sqlx::{MySql, Row as _, Transaction};
use tonic::Status;

use crate::idem::{self, Claimed};
use crate::pb::yadgar::common::v1::{Meta, Scope, Visibility};
use crate::pb::yadgar::task::v1::*;
use crate::service::TaskDb;
use crate::sql::{internal, scope_of, Reach};

/// What `UpdateTask` says when its compare-and-set matches no row.
///
/// **THE ADVICE IS THE LOAD-BEARING HALF, AND ENFORCEMENT BROKE IT.** The
/// message used to end "re-read and retry", which was followable while the
/// three causes agreed: a caller who could not edit a record could not read it
/// either, so the re-read failed and the caller stopped. ADR-0522 grants an
/// owner outside the team of their own record the READ and deliberately does
/// not widen the edit ([`Reach::predicate`] carries no ADR-0522 arm), so the
/// re-read now succeeds and returns the SAME version. A caller obeying the old
/// advice re-sent an identical request forever.
///
/// So the advice is CONDITIONAL on something the caller can observe, and it
/// covers every outcome a re-read has: the version moved, the version did not,
/// or there was nothing to return. Only the first is worth retrying.
///
/// **IT NAMES A THIRD CAUSE AND CONFIRMS NONE OF THEM.** Three disjuncts
/// disclose no more than two did: a caller who reaches the record already knows
/// it exists, and one who does not gets `NOT_FOUND` from the re-read. Whether
/// the row is there is not decidable from this string.
const UPDATE_REFUSED: &str = "version mismatch, no such task in this scope, or a task this \
     caller may read but not edit. Re-read: if the version has moved, retry with the new one. \
     If the read returns the same version, or returns nothing, retrying will fail identically.";

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
        if let Claimed::Replay(original) = idem::claim(
            &mut tx,
            scope,
            "CreateTask",
            req.idempotency.as_ref(),
            || req.payload(),
        )
        .await?
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

    /// The next number for a project, allocated ON THE CALLER'S TRANSACTION and
    /// on nothing else.
    ///
    /// **ONE `CreateTask` MUST USE ONE CONNECTION.** This took two: it opened
    /// with an `INSERT IGNORE` executed against `&self.pool` — a second acquire
    /// from the same pool while `create`'s transaction still held the first. So N
    /// concurrent creates against a pool of N take every connection with their
    /// transactions and then all wait for a connection only one of them can
    /// release. Measured at pool size 4 against MariaDB 11.8: one of four creates
    /// succeeded and three failed after the full acquire timeout. The production
    /// default is eight, and D80 made it a value an operator can lower — which
    /// `check_engine_headroom` actively steers a constrained operator to do.
    ///
    /// The wait is also not a database error, so it never reached the retryable
    /// arm of [`internal`]: `sqlx::Error::PoolTimedOut` is its own variant and
    /// answered `INTERNAL`, telling a caller that a transient stall is permanent.
    /// That arm is fixed there as well, because an unreachable defect is still a
    /// defect.
    ///
    /// **UPSERT-AND-INCREMENT, in one statement.** `ON DUPLICATE KEY UPDATE`
    /// takes an EXCLUSIVE lock on the row it collides with, so concurrent
    /// allocators queue behind each other and the first to commit is the one the
    /// next reads.
    ///
    /// It is deliberately not `INSERT IGNORE` moved onto the transaction, which
    /// is the smaller-looking change. An `INSERT IGNORE` that collides takes a
    /// SHARED lock on the duplicate row, and the `SELECT ... FOR UPDATE` after it
    /// asks to upgrade that shared lock to an exclusive one. With two racers only
    /// one holds the shared lock and the upgrade succeeds; with THREE OR MORE,
    /// two hold it and each waits for the other to let go.
    ///
    /// MEASURED against MariaDB 11.8, that shape on one transaction, fresh
    /// project: two racers passed twelve rounds, and FOUR racers failed on round
    /// zero with a deadlock the engine broke by killing a create. Two racers is
    /// what this suite had, so two racers is the number that proves nothing.
    ///
    /// Reading first and inserting only when the read comes back empty remains
    /// wrong for the reason it always was: a `SELECT ... FOR UPDATE` matching
    /// nothing leaves a GAP lock, and the insert meant to fill the gap waits on
    /// the transaction about to perform it. There is no locking read here at all,
    /// so that shape cannot come back.
    ///
    /// The read-back needs no `FOR UPDATE`. The statement above already holds the
    /// row exclusively until this transaction commits, and a plain `SELECT` in
    /// the same transaction sees that transaction's own write.
    async fn allocate_number(
        &self,
        tx: &mut Transaction<'_, MySql>,
        project: &str,
    ) -> Result<u32, Status> {
        sqlx::query(
            "INSERT INTO task_counter (project_id, next_number) VALUES (?, 1)
             ON DUPLICATE KEY UPDATE next_number = next_number + 1",
        )
        .bind(project)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;

        let number: u32 =
            sqlx::query_scalar("SELECT next_number FROM task_counter WHERE project_id = ?")
                .bind(project)
                .fetch_one(&mut **tx)
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
        assigned_by_the_module(task.meta.as_ref())?;
        let fields = fields_of(req.update_mask.as_ref())?;
        let reach = Reach::of(scope);

        let mut tx = self.pool.begin().await.map_err(internal)?;
        if let Claimed::Replay(original) = idem::claim(
            &mut tx,
            scope,
            "UpdateTask",
            req.idempotency.as_ref(),
            || req.payload(),
        )
        .await?
        {
            tx.rollback().await.map_err(internal)?;
            return Ok(original);
        }

        // THE PRIOR STATUS, read BEFORE the update and inside the same
        // transaction. `UpdateTaskResponse.previous_status` promises the status
        // the row held before this write was applied, and the update itself is
        // what destroys that value — so it is read here or it is not readable
        // at all. `iam`-style recomputation is not available to the caller
        // either: the logic service reads the task first and gets what the
        // FIRST attempt wrote, which is the defect the field exists to close.
        //
        // ON EVERY UPDATE, NOT ONLY ON A STATUS CHANGE. An `update_mask` that
        // does not name `status` still displaces a status, and the field
        // carries it — equal to the status the row holds afterwards, which is
        // the truth rather than an omission. TASK_STATUS_UNSPECIFIED must
        // therefore have exactly one cause after this ships: an idempotency row
        // recorded before the field existed. Making a mask that omits `status`
        // a second cause is what this read exists to prevent.
        //
        // `FOR UPDATE`, and the lock is the point rather than the read. It
        // holds the row from here until commit, so the value recorded is the
        // one this write actually displaced and not one a concurrent writer
        // replaced in between. It also sees the latest committed row rather
        // than this transaction's snapshot.
        //
        // AUDIT: the interpolation is this module's own predicate; every caller
        // value is a bound parameter.
        let sql = format!(
            "SELECT status FROM task
              WHERE id = ? AND version = ? AND deleted_at IS NULL AND {}
              FOR UPDATE",
            reach.predicate()
        );
        let query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&req.id)
            .bind(req.expect_version);
        let found = reach
            .bind(query)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;

        // The same refusal the compare-and-set below reaches, arrived at one
        // statement earlier. The two conditions are identical — same id, same
        // version, same reach, same `deleted_at` — so a caller cannot tell
        // which statement refused, and both render `UPDATE_REFUSED`. It is a
        // constant rather than two literals for exactly that reason: the
        // indistinguishability is the property, and two literals are two places
        // for it to stop being true.
        let Some(row) = found else {
            tx.rollback().await.map_err(internal)?;
            return Err(Status::failed_precondition(UPDATE_REFUSED));
        };
        let previous_status = row.try_get::<i8, _>("status").map_err(internal)? as i32;

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
            return Err(Status::failed_precondition(UPDATE_REFUSED));
        }

        // `previous_status` is part of the recorded response, so `idem::record`
        // below persists it and a replay returns the value the FIRST attempt
        // displaced rather than one recomputed from a row that has since moved.
        // That is the whole of the replay guarantee the field's comment makes,
        // and it needs no change to the idempotency ledger.
        let response = UpdateTaskResponse {
            meta: Some(Meta {
                id: req.id.clone(),
                version: req.expect_version + 1,
                project_id: scope.project_id.clone(),
                ..Default::default()
            }),
            previous_status,
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
        if let Claimed::Replay(original) = idem::claim(
            &mut tx,
            scope,
            "DeleteTask",
            req.idempotency.as_ref(),
            || req.payload(),
        )
        .await?
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

/// The bytes D9's fingerprint is taken over.
///
/// Which fields count as the payload is per-RPC, and D9 requires that written
/// down rather than inferred. This module's answer is the same for all three of
/// its mutating RPCs: **every field of the request except `scope` and
/// `idempotency`** — see the header of `src/idem.rs` for why those two, and only
/// those two, are excluded.
///
/// Stated as "clear the two, keep the rest" rather than as a list of the fields
/// to hash. The list would be the same today and would silently stop being
/// right the day the contract grows a field: a new one nobody added here would
/// be omitted from the digest, and a request differing only in it would be
/// replayed. The exclusions are what this module has actually decided about.
trait Payload {
    fn payload(&self) -> Vec<u8>;
}

impl Payload for CreateTaskRequest {
    fn payload(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.scope = None;
        canonical.idempotency = None;
        canonical.encode_to_vec()
    }
}

impl Payload for UpdateTaskRequest {
    fn payload(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.scope = None;
        canonical.idempotency = None;
        // A mask is a SET of field names, and its encoding is a sequence. Two
        // masks naming the same fields in another order, or one naming a field
        // twice, ask for the identical write — so a digest taken over the bytes
        // as they arrived would refuse a request that discards nothing. D9's
        // test is whether a replay would silently discard the difference.
        if let Some(mask) = canonical.update_mask.as_mut() {
            mask.paths.sort();
            mask.paths.dedup();
        }
        canonical.encode_to_vec()
    }
}

impl Payload for DeleteTaskRequest {
    fn payload(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.scope = None;
        canonical.idempotency = None;
        canonical.encode_to_vec()
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
///
/// The three TIMESTAMPS are checked for the same reason and were missing from
/// this list. `insert` never binds them — the engine defaults `created_at` and
/// `updated_at`, and `deleted_at` is a tombstone D26 places later — so a caller
/// that set one reached no column and was told OK. Nothing wrong was stored,
/// and that is not the standard: the caller asked for something, the module
/// discarded it, and the answer reported success. Every field of `Meta` is
/// assigned here, so every field of `Meta` is refused here.
///
/// **Called from EVERY write path, and `update` did not call it.** The argument
/// above is about the ANSWER, not about the column, so it does not stop at
/// create: an update carrying a `Meta` reaches no column either — `Field::ALL`
/// is title, body, status, tags and links — and was told OK just the same. The
/// exposure was never the data; it was a request discarded behind a success.
/// Reading this guard as create-only is what let the update path keep the defect
/// the create path had already fixed.
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
        || meta.created_at.is_some()
        || meta.updated_at.is_some()
        || meta.deleted_at.is_some()
        || !meta.derived_from.is_empty();
    if supplied {
        return Err(Status::invalid_argument(
            "meta is assigned by this module and must be empty on a write (D42)",
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

    /// What an update wrote before masks were honoured and before `tags` and
    /// `links` had columns — which is exactly the set an unmasked caller can
    /// know about. See [`fields_of`].
    const BEFORE_THE_MASK: [Field; 3] = [Field::Title, Field::Body, Field::Status];

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

/// An absent or empty mask means the fields an update wrote BEFORE the mask was
/// honoured — title, body and status — and deliberately not `tags` or `links`.
///
/// "Absent means everything" is the obvious reading and it loses data during a
/// rollout. A caller built against the older contract cannot populate `tags`,
/// so its request carries the empty vec that is the field's zero value; treating
/// that as an instruction would erase a task's tags on every status change made
/// by a pod that has not been upgraded yet. A caller that wants to write them
/// names them, which an old caller cannot do and a new one always does.
///
/// A mask that NAMES fields is honoured, and that is what lets `EditTask` write
/// a title without also writing the status it had to read first.
fn fields_of(mask: Option<&FieldMask>) -> Result<Vec<Field>, Status> {
    let Some(mask) = mask.filter(|m| !m.paths.is_empty()) else {
        return Ok(Field::BEFORE_THE_MASK.to_vec());
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
