//! `GetTask` and `ListTasks`.

use tonic::Status;

use crate::pb::yadgar::task::v1::*;
use crate::rows::{row_to_task, COLUMNS};
use crate::service::TaskDb;
use crate::sql::{internal, scope_of, Reach};

/// D56 bounds reads: an unbounded page is how one caller takes the whole table.
const MAX_PAGE: i32 = 500;
const DEFAULT_PAGE: i32 = 50;

impl TaskDb {
    pub(crate) async fn get(&self, req: GetTaskRequest) -> Result<GetTaskResponse, Status> {
        let scope = scope_of(&req.scope)?;
        let reach = Reach::of(scope);

        // Scope is part of the WHERE, not a check after the fact. A row the
        // caller may not see must not be fetched and then filtered — the
        // difference matters the day someone logs the pre-filter result.
        let row = match req.key {
            Some(get_task_request::Key::Id(id)) => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM task
                      WHERE id = ? AND deleted_at IS NULL AND {}",
                    reach.predicate()
                );
                // AUDIT: the interpolation is this module's own predicate; every
                // caller value is a bound parameter.
                reach
                    .bind(sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id))
                    .fetch_optional(&self.pool)
                    .await
            }
            Some(get_task_request::Key::Number(number)) => {
                // A number is unique WITHIN a project, so this arm pins the
                // project by equality. The visibility axis still applies: a
                // filter added to one arm and not the other is precisely the
                // shape of the leak this replaced.
                let sql = format!(
                    "SELECT {COLUMNS} FROM task
                      WHERE number = ? AND deleted_at IS NULL AND project_id = ? AND {}",
                    reach.visible()
                );
                // AUDIT: as above.
                reach
                    .bind_visible(
                        sqlx::query(sqlx::AssertSqlSafe(sql))
                            .bind(number)
                            .bind(reach.project()),
                    )
                    .fetch_optional(&self.pool)
                    .await
            }
            None => return Err(Status::invalid_argument("one of id or number is required")),
        }
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("no such task in this scope"))?;

        Ok(GetTaskResponse {
            task: Some(row_to_task(&row)?),
        })
    }

    pub(crate) async fn list(&self, req: ListTasksRequest) -> Result<ListTasksResponse, Status> {
        let scope = scope_of(&req.scope)?;
        let reach = Reach::of(scope);
        let limit = page_size(req.page_size);
        let statuses = statuses(&req.statuses)?;

        let mut sql = format!(
            "SELECT {COLUMNS} FROM task WHERE deleted_at IS NULL AND {}",
            reach.predicate()
        );
        if !statuses.is_empty() {
            let holes = vec!["?"; statuses.len()].join(", ");
            sql.push_str(&format!(" AND status IN ({holes})"));
        }
        if !req.page_token.is_empty() {
            // Keyset, not OFFSET. The id is a UUIDv7 URN, so it is already the
            // sort key and a page boundary is a value rather than a count —
            // which is what keeps a row from being skipped or repeated when the
            // set changes between pages.
            sql.push_str(" AND id > ?");
        }
        // One more than asked for, purely to learn whether a next page exists.
        // Returning a token unconditionally would read as "there is more" on the
        // final page and cost the caller a round trip to find out otherwise.
        sql.push_str(" ORDER BY id LIMIT ?");

        // AUDIT: the interpolations are this module's predicate and a count of
        // `?` placeholders; every caller value is bound.
        let mut query = reach.bind(sqlx::query(sqlx::AssertSqlSafe(sql)));
        for status in &statuses {
            query = query.bind(status);
        }
        if !req.page_token.is_empty() {
            query = query.bind(&req.page_token);
        }
        let mut rows = query
            .bind(i64::from(limit) + 1)
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;

        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let tasks = rows
            .iter()
            .map(row_to_task)
            .collect::<Result<Vec<_>, _>>()?;

        let next_page_token = match tasks.last() {
            Some(last) if more => last.meta.as_ref().map(|m| m.id.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(ListTasksResponse {
            tasks,
            next_page_token,
        })
    }
}

/// A page size the caller did not set is 0, which would return nothing and look
/// like an empty store. Bounded above as well (D56).
fn page_size(requested: i32) -> i32 {
    match requested {
        n if n <= 0 => DEFAULT_PAGE,
        n if n > MAX_PAGE => MAX_PAGE,
        n => n,
    }
}

/// An empty filter means EVERY status, not none — a caller that sets no filter
/// is not asking for an empty page.
fn statuses(requested: &[i32]) -> Result<Vec<i8>, Status> {
    requested
        .iter()
        .map(|s| {
            TaskStatus::try_from(*s)
                .map(|s| s as i8)
                // Silently dropping an unrecognised value would answer a
                // question the caller did not ask, with a page that looks
                // authoritative.
                .map_err(|_| Status::invalid_argument(format!("{s} is not a TaskStatus")))
        })
        .collect()
}
