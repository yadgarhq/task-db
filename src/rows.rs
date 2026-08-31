//! One row, one `Task`.

use sqlx::types::Json;
use sqlx::Row;
use tonic::Status;

use crate::pb::yadgar::common::v1::Meta;
use crate::pb::yadgar::task::v1::Task;
use crate::sql::internal;

/// Named once, so the two read paths cannot drift into selecting different
/// columns and `row_to_task` cannot ask for one that a statement forgot.
pub const COLUMNS: &str =
    "id, version, project_id, owner_user_id, number, title, body, status, tags, links";

pub fn row_to_task(row: &sqlx::mysql::MySqlRow) -> Result<Task, Status> {
    let strings = |name| {
        row.try_get::<Json<Vec<String>>, _>(name)
            .map(|j| j.0)
            .map_err(internal)
    };
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
        tags: strings("tags")?,
        links: strings("links")?,
    })
}
