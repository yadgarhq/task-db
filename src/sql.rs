//! How a caller's reach becomes a `WHERE` clause, and how an engine error
//! becomes a status.
//!
//! Everything here exists because scope is enforced in the STATEMENT rather than
//! after it. A row the caller may not see must never be fetched and then
//! filtered — the difference matters the day someone logs the pre-filter result,
//! and it is the difference between a leak and a bug.

use sqlx::mysql::MySqlArguments;
use sqlx::query::Query;
use sqlx::MySql;
use tonic::Status;

use crate::pb::yadgar::common::v1::{Scope, Visibility};

/// The gateway attests scope from credentials; it is never supplied by the
/// caller (D12). An absent scope is a programming error upstream, not a
/// permissive default — so it is refused rather than treated as "everything".
pub fn scope_of(scope: &Option<Scope>) -> Result<&Scope, Status> {
    scope
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("scope is required and is attested by the gateway"))
}

/// Copy the scope fields a record needs, before the request is consumed.
///
/// Empty strings when the scope is absent: the call is refused on its own merits,
/// and telemetry must never be the thing that fails a request (D25).
pub fn tel_scope(scope: &Option<Scope>) -> yadgar_telemetry::observe::Scope {
    let field = |f: fn(&Scope) -> &String| scope.as_ref().map(f).cloned().unwrap_or_default();
    yadgar_telemetry::observe::Scope {
        request_id: field(|s| &s.request_id),
        instance_id: field(|s| &s.instance_id),
        user_id: field(|s| &s.user_id),
        project_id: field(|s| &s.project_id),
    }
}

/// Neutralise the LIKE metacharacters in a value that is about to become part of
/// a pattern.
///
/// A bound parameter stops SQL injection; it does NOT stop PATTERN injection,
/// and a project id is caller data reaching a pattern language. `_` matches any
/// single character, so an unescaped `acme_team/%` also matches
/// `acmeXteam/secret` — a project nobody granted. `%` is the same hole at its
/// widest: a caller scoped to `%` is handed the table.
///
/// The backslash goes FIRST. Escaping it after the others would escape the
/// escapes.
fn like_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// D53: a project id is a hierarchical path and matches its own subtree, so a
/// query at "quinyx/qwfm" reaches "quinyx/qwfm/forecast". Equality would silently
/// return nothing for a caller scoped to an ancestor.
///
/// The `/%` is appended AFTER escaping, so the wildcard this pattern is supposed
/// to have survives while the ones the caller smuggled in do not.
pub fn subtree(project_id: &str) -> String {
    format!("{}/%", like_escape(project_id))
}

/// Spelled out rather than left to MariaDB's default, which `NO_BACKSLASH_ESCAPES`
/// changes underneath a statement that assumed it.
const ESCAPE: &str = r"ESCAPE '\\'";

/// What one caller can reach, on both axes.
///
/// Scoping is TWO-DIMENSIONAL and the axes are independent (D53): visibility
/// says WHO may see a record, project depth says WHERE it lives, and a record
/// has to pass on both. Collapsing them into one ladder is the plausible-looking
/// mistake, so they are two predicates joined by AND and never one.
pub struct Reach {
    project: String,
    subtree: String,
    user: String,
    teams: Vec<String>,
}

impl Reach {
    pub fn of(scope: &Scope) -> Self {
        Self {
            project: scope.project_id.clone(),
            subtree: subtree(&scope.project_id),
            user: scope.user_id.clone(),
            teams: scope.team_ids.clone(),
        }
    }

    /// The predicate for both axes. Its parameters are bound by [`Reach::bind`],
    /// in this order, so it belongs LAST in any statement that carries other
    /// bindings.
    pub fn predicate(&self) -> String {
        format!("{} AND {}", self.within(), self.visible())
    }

    /// The project axis ALONE, for `DeleteTask`. D26 makes delete owner-only,
    /// which is already narrower than the ladder — and ANDing the ladder on top
    /// would lock an owner out of their own record the day they leave the team
    /// it is shared with.
    pub fn within(&self) -> String {
        format!("(project_id = ? OR project_id LIKE ? {ESCAPE})")
    }

    /// The D12 ladder: PRIVATE is the owner's, TEAM is the named team's, ORG is
    /// everyone's.
    ///
    /// There is deliberately NO `OR owner_user_id = ?` arm spanning the whole
    /// clause. It reads like a safe addition — surely an owner may see their own
    /// record — and it would quietly make every TEAM test pass for the wrong
    /// reason. The PRIVATE rung already grants the owner what the owner needs.
    /// The visibility axis ALONE, for the one statement that pins the project
    /// by equality rather than by subtree: a `number` is unique within its own
    /// project, and matching the subtree there would let an ancestor scope find
    /// several different tasks numbered 1 and return whichever came first.
    pub fn visible(&self) -> String {
        let (team, org) = (Visibility::Team as i8, Visibility::Org as i8);
        // An empty team list drops the TEAM arm entirely. Rendering `IN ()` is a
        // syntax error, and rendering the arm without it is worse: `visibility =
        // 2` alone would make every team record visible to everybody.
        let team_arm = if self.teams.is_empty() {
            String::new()
        } else {
            let holes = vec!["?"; self.teams.len()].join(", ");
            format!(" OR (visibility = {team} AND team_id IN ({holes}))")
        };
        // The PRIVATE rung is "anything that is not one of the wider two",
        // rather than `visibility = 1`, and that is the difference between
        // failing closed and failing INVISIBLE. `unwrap_or(1)` used to accept
        // VISIBILITY_UNSPECIFIED, so rows carrying 0 may exist; against three
        // equality arms such a row matches none of them and becomes unreadable
        // by everyone including its owner — a quiet way to lose data that no
        // test starting from an empty table can see. Migration 5 heals the rows
        // themselves; this makes any value nobody anticipated land on the most
        // restrictive rung that still has an owner, which is what D12 says the
        // default is.
        format!("(visibility = {org} OR (visibility NOT IN ({team}, {org}) AND owner_user_id = ?){team_arm})")
    }

    /// Bind what [`Reach::predicate`] left holes for, in the same order.
    pub fn bind<'q>(
        &'q self,
        query: Query<'q, MySql, MySqlArguments>,
    ) -> Query<'q, MySql, MySqlArguments> {
        self.bind_visible(self.bind_within(query))
    }

    /// The parameters of [`Reach::within`], in the same order.
    pub fn bind_within<'q>(
        &'q self,
        query: Query<'q, MySql, MySqlArguments>,
    ) -> Query<'q, MySql, MySqlArguments> {
        query.bind(&self.project).bind(&self.subtree)
    }

    /// The parameters of [`Reach::visible`], in the same order.
    pub fn bind_visible<'q>(
        &'q self,
        query: Query<'q, MySql, MySqlArguments>,
    ) -> Query<'q, MySql, MySqlArguments> {
        let mut query = query.bind(&self.user);
        for team in &self.teams {
            query = query.bind(team);
        }
        query
    }

    pub fn project(&self) -> &str {
        &self.project
    }
}

/// An engine error is never returned to the caller verbatim: it carries table
/// names, column names and sometimes values. Logged here, generic on the wire.
///
/// EXCEPT for the one class that the caller can act on. A deadlock or a lock
/// wait timeout means "someone else held the row; try again", and flattening it
/// into `INTERNAL` tells a caller that a retryable wait is a permanent failure —
/// which is how a moment of contention becomes a user-visible error. `ABORTED`
/// is the code gRPC reserves for exactly this.
pub fn internal(e: sqlx::Error) -> Status {
    if let sqlx::Error::Database(db) = &e {
        // 1213 is ER_LOCK_DEADLOCK and 1205 ER_LOCK_WAIT_TIMEOUT. SQLSTATE
        // 40001 covers the first; the second reports HY000 and is only
        // identifiable by its number.
        let deadlock = db.code().as_deref() == Some("40001")
            || db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .is_some_and(|e| matches!(e.number(), 1205 | 1213));
        if deadlock {
            tracing::warn!(error = %e, "task-db lock contention; the caller may retry");
            return Status::aborted("the write could not be serialised — retry");
        }
    }
    tracing::error!(error = %e, "task-db engine error");
    Status::internal("storage error")
}
