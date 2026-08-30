//! `TaskDbService`. One RPC is one transaction (D5), and scope is enforced on
//! every path rather than trusted from the caller.

use sqlx::{MySqlPool, Row};
use tonic::{Request, Response, Status};

use crate::pb::yadgar::common::v1::{Meta, Scope};
use crate::pb::yadgar::task::v1::task_db_service_server::TaskDbService;
use crate::pb::yadgar::task::v1::*;

pub struct TaskDb {
    pool: MySqlPool,
}

impl TaskDb {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }
}

/// The gateway attests scope from credentials; it is never supplied by the
/// caller (D12). An absent scope is a programming error upstream, not a
/// permissive default — so it is refused rather than treated as "everything".
fn scope_of(scope: &Option<Scope>) -> Result<&Scope, Status> {
    scope
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("scope is required and is attested by the gateway"))
}

/// D53: a project id is a hierarchical path and matches its own subtree, so a
/// query at "quinyx/qwfm" reaches "quinyx/qwfm/forecast". Equality would silently
/// return nothing for a caller scoped to an ancestor.
fn subtree(project_id: &str) -> String {
    format!("{project_id}/%")
}

#[tonic::async_trait]
impl TaskDbService for TaskDb {
    async fn create_task(
        &self,
        request: Request<CreateTaskRequest>,
    ) -> Result<Response<CreateTaskResponse>, Status> {
        let req = request.into_inner();
        let scope = scope_of(&req.scope)?;
        let task = req
            .task
            .ok_or_else(|| Status::invalid_argument("task is required"))?;

        // UUIDv7: time-ordered, so keyset pagination and index locality behave
        // (D42). The URN is what leaves this service; the raw uuid never does.
        let id = format!("yadgar:task:{}", uuid::Uuid::now_v7());

        let mut tx = self.pool.begin().await.map_err(internal)?;

        // The number is per-project and allocated inside the same transaction as
        // the insert. Reading a MAX and inserting afterwards in two statements is
        // a race that hands two concurrent creates the same number; the UNIQUE on
        // (project_id, number) would then reject one of them, which is safe but
        // presents as a random failure. Holding it in one transaction is the fix.
        let number: u32 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(MAX(number), 0) + 1 AS UNSIGNED)
             FROM task WHERE project_id = ? FOR UPDATE",
        )
        .bind(&scope.project_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;

        sqlx::query(
            "INSERT INTO task
               (id, project_id, owner_user_id, team_id, visibility,
                created_by, updated_by, number, title, body, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&scope.project_id)
        .bind(&scope.user_id)
        .bind(task.meta.as_ref().map(|m| m.team_id.as_str()).unwrap_or(""))
        .bind(task.meta.as_ref().map(|m| m.visibility).unwrap_or(1) as i8)
        .bind(&scope.user_id)
        .bind(&scope.user_id)
        .bind(number)
        .bind(&task.title)
        .bind(&task.body)
        .bind(task.status as i8)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

        tx.commit().await.map_err(internal)?;

        Ok(Response::new(CreateTaskResponse {
            meta: Some(Meta {
                id,
                version: 1,
                project_id: scope.project_id.clone(),
                owner_user_id: scope.user_id.clone(),
                ..Default::default()
            }),
            number,
        }))
    }

    async fn get_task(
        &self,
        request: Request<GetTaskRequest>,
    ) -> Result<Response<GetTaskResponse>, Status> {
        let req = request.into_inner();
        let scope = scope_of(&req.scope)?;

        // Scope is part of the WHERE, not a check after the fact. A row the
        // caller may not see must not be fetched and then filtered — the
        // difference matters the day someone logs the pre-filter result.
        let row = match req.key {
            Some(get_task_request::Key::Id(id)) => {
                sqlx::query(
                    "SELECT id, version, project_id, owner_user_id, number, title, body, status
                     FROM task
                     WHERE id = ? AND deleted_at IS NULL
                       AND (project_id = ? OR project_id LIKE ?)",
                )
                .bind(id)
                .bind(&scope.project_id)
                .bind(subtree(&scope.project_id))
                .fetch_optional(&self.pool)
                .await
            }
            Some(get_task_request::Key::Number(number)) => {
                sqlx::query(
                    "SELECT id, version, project_id, owner_user_id, number, title, body, status
                     FROM task
                     WHERE number = ? AND deleted_at IS NULL AND project_id = ?",
                )
                .bind(number)
                .bind(&scope.project_id)
                .fetch_optional(&self.pool)
                .await
            }
            None => return Err(Status::invalid_argument("one of id or number is required")),
        }
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("no such task in this scope"))?;

        Ok(Response::new(GetTaskResponse {
            task: Some(row_to_task(&row)?),
        }))
    }

    async fn list_tasks(
        &self,
        request: Request<ListTasksRequest>,
    ) -> Result<Response<ListTasksResponse>, Status> {
        let req = request.into_inner();
        let scope = scope_of(&req.scope)?;

        // A page size the caller did not set is 0, which would return nothing and
        // look like an empty store. Bounded above as well: an unbounded page is
        // how one caller takes the whole table (D56 bounds reads).
        let limit = match req.page_size {
            n if n <= 0 => 50,
            n if n > 500 => 500,
            n => n,
        } as i64;

        let rows = sqlx::query(
            "SELECT id, version, project_id, owner_user_id, number, title, body, status
             FROM task
             WHERE deleted_at IS NULL AND (project_id = ? OR project_id LIKE ?)
             ORDER BY id
             LIMIT ?",
        )
        .bind(&scope.project_id)
        .bind(subtree(&scope.project_id))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;

        let tasks = rows
            .iter()
            .map(row_to_task)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(ListTasksResponse {
            tasks,
            next_page_token: String::new(),
        }))
    }

    async fn update_task(
        &self,
        request: Request<UpdateTaskRequest>,
    ) -> Result<Response<UpdateTaskResponse>, Status> {
        let req = request.into_inner();
        let scope = scope_of(&req.scope)?;
        let task = req
            .task
            .ok_or_else(|| Status::invalid_argument("task is required"))?;

        // Compare-and-set (D8). The version is in the WHERE, so a concurrent
        // writer's update makes this one match zero rows rather than silently
        // overwriting. FAILED_PRECONDITION is the contract's answer.
        let result = sqlx::query(
            "UPDATE task
                SET title = ?, body = ?, status = ?, version = version + 1, updated_by = ?
              WHERE id = ? AND version = ? AND deleted_at IS NULL
                AND (project_id = ? OR project_id LIKE ?)",
        )
        .bind(&task.title)
        .bind(&task.body)
        .bind(task.status as i8)
        .bind(&scope.user_id)
        .bind(&req.id)
        .bind(req.expect_version)
        .bind(&scope.project_id)
        .bind(subtree(&scope.project_id))
        .execute(&self.pool)
        .await
        .map_err(internal)?;

        if result.rows_affected() == 0 {
            return Err(Status::failed_precondition(
                "version mismatch, or no such task in this scope — re-read and retry",
            ));
        }

        Ok(Response::new(UpdateTaskResponse {
            meta: Some(Meta {
                id: req.id,
                version: req.expect_version + 1,
                project_id: scope.project_id.clone(),
                ..Default::default()
            }),
        }))
    }

    async fn delete_task(
        &self,
        request: Request<DeleteTaskRequest>,
    ) -> Result<Response<DeleteTaskResponse>, Status> {
        let req = request.into_inner();
        let scope = scope_of(&req.scope)?;

        // Soft, and OWNER-ONLY (D26). The owner check is in the statement rather
        // than a read-then-decide, so it cannot race with a change of owner.
        let result = sqlx::query(
            "UPDATE task SET deleted_at = CURRENT_TIMESTAMP, version = version + 1
              WHERE id = ? AND version = ? AND deleted_at IS NULL
                AND owner_user_id = ?
                AND (project_id = ? OR project_id LIKE ?)",
        )
        .bind(&req.id)
        .bind(req.expect_version)
        .bind(&scope.user_id)
        .bind(&scope.project_id)
        .bind(subtree(&scope.project_id))
        .execute(&self.pool)
        .await
        .map_err(internal)?;

        if result.rows_affected() == 0 {
            return Err(Status::failed_precondition(
                "version mismatch, not the owner, or no such task in this scope",
            ));
        }
        Ok(Response::new(DeleteTaskResponse {}))
    }
}

fn row_to_task(row: &sqlx::mysql::MySqlRow) -> Result<Task, Status> {
    Ok(Task {
        meta: Some(Meta {
            id: row.try_get("id").map_err(internal)?,
            version: row.try_get("version").map_err(internal)?,
            project_id: row.try_get("project_id").map_err(internal)?,
            owner_user_id: row.try_get("owner_user_id").map_err(internal)?,
            ..Default::default()
        }),
        number: row.try_get("number").map_err(internal)?,
        title: row.try_get("title").map_err(internal)?,
        body: row.try_get("body").map_err(internal)?,
        status: row.try_get::<i8, _>("status").map_err(internal)? as i32,
        tags: Vec::new(),
        links: Vec::new(),
    })
}

/// An engine error is never returned to the caller verbatim: it carries table
/// names, column names and sometimes values. Logged here, generic on the wire.
fn internal<E: std::fmt::Display>(e: E) -> Status {
    tracing::error!(error = %e, "task-db engine error");
    Status::internal("storage error")
}
