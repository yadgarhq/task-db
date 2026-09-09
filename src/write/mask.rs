//! The columns an `UpdateTask` may write, and how an `update_mask` names them.
//!
//! Split out of `write.rs` when that file crossed the shared 500-line ceiling.
//! Every line below stood there unchanged. It is a vocabulary rather than a
//! step of the RPC: nothing here reaches a pool, a transaction or
//! [`crate::service::TaskDb`].

use prost_types::FieldMask;
use sqlx::mysql::MySqlArguments;
use sqlx::query::Query;
use sqlx::types::Json;
use sqlx::MySql;
use tonic::Status;

use crate::pb::yadgar::task::v1::*;

/// The columns an update may write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
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

    pub(super) fn column(self) -> &'static str {
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

    pub(super) fn bind<'q>(
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
pub(super) fn fields_of(mask: Option<&FieldMask>) -> Result<Vec<Field>, Status> {
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
