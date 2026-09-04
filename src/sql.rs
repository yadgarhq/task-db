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

use crate::pb::yadgar::common::v1::{InheritedSetting, Scope, Visibility};
use crate::setting::owner_reach;

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
    /// ADR-0522's setting, carried UNRESOLVED. The answer depends on the team
    /// of the row, so the inputs travel this far and are resolved by
    /// [`Reach::readable`] and nowhere else.
    setting: Option<InheritedSetting>,
}

impl Reach {
    pub fn of(scope: &Scope) -> Self {
        Self {
            project: scope.project_id.clone(),
            subtree: subtree(&scope.project_id),
            user: scope.user_id.clone(),
            teams: scope.team_ids.clone(),
            setting: scope.owner_reads_own_record.clone(),
        }
    }

    /// The predicate for both axes, WITHOUT ADR-0522's widening.
    ///
    /// **THIS IS THE EDIT PATH'S PREDICATE AND `UpdateTask` IS ITS ONLY CALLER.
    /// A READ WANTS [`Reach::read_predicate`].** The setting is named
    /// `owner_reads_own_record` and `Scope` scopes it to reads — "whether an
    /// owner READS their own record" — so widening an UPDATE with it would hand
    /// out edit authority from a setting whose name promises a read. That is
    /// the same class of silent widening the setting exists to prevent, one
    /// step worse, so the edit path keeps the ladder it had.
    ///
    /// The invariant `UpdateTask` states — "a record the caller may not see is
    /// also one it cannot edit" — is one-directional and survives: reads
    /// widening while edits do not SHRINKS the unseeable set, and everything
    /// unseeable is still uneditable. D26 already makes `DeleteTask` narrower
    /// than the ladder, so a mutating path narrower than a read is this
    /// module's existing shape rather than a new one.
    pub fn predicate(&self) -> String {
        format!("{} AND {}", self.within(), self.visible())
    }

    /// The predicate for both axes AS A READ SEES THEM, which is where
    /// ADR-0522 applies.
    ///
    /// Fallible because the setting can name no policy, and a store may not
    /// choose one on its behalf. Its parameters are bound by [`Reach::bind`]
    /// FOLLOWED BY [`Readable::bind`], in that order.
    pub fn read_predicate(&self) -> Result<Readable, Status> {
        let readable = self.readable()?;
        Ok(Readable {
            sql: format!("{} AND {}", self.within(), readable.sql),
            binds: readable.binds,
        })
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
    /// There is STILL deliberately no unconditional `OR owner_user_id = ?` arm
    /// spanning the whole clause, and ADR-0522 did not make one safe. It reads
    /// like a safe addition — surely an owner may see their own record — and it
    /// would quietly make every TEAM test pass for the wrong reason. What
    /// ADR-0522 adds is an arm the SETTING gates, resolved against the team of
    /// the ROW, and it is added by [`Reach::readable`] rather than here: a
    /// blanket arm reaches the same rows in the shipped configuration and
    /// reaches them for a reason nobody stated, which is why `tests/scope.rs`
    /// still pins it as refused.
    ///
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

    /// The visibility axis AS A READ SEES IT: the D12 ladder, plus ADR-0522's
    /// arm where the setting grants one.
    ///
    /// **THIS IS THE CALL SITE THAT PUTS THIS SERVICE IN THE ENFORCING STATE.**
    /// `yadgar/common/v1` admits exactly two states and names the
    /// discriminator — "WHICH ONE IT IS IN IS DECIDED BY WHETHER IT READS THIS
    /// FIELD" — and this reads it. The refusal therefore binds absolutely: an
    /// absent setting and a present one stating nothing are one case, both
    /// refused, and there is no third state in which this tolerates an unset
    /// one. A default substituted here is the silent wrong policy the whole
    /// setting exists to prevent.
    ///
    /// **THE ARM IS COMPOSED ONTO THE LADDER, NEVER A SECOND LADDER.** The
    /// clause is literally [`Reach::visible`] with one `OR` appended, so there
    /// is one statement of D12 in this module and an additive exception beside
    /// it. Two ladders would be two answers to one question, and the one that
    /// drifts is the one a reader happens to open.
    ///
    /// **THE ARM IS KEYED ON THE ROW'S `team_id`, WHICH IS THE WHOLE POINT.**
    /// `owner_reach` answers for every team the setting can answer differently
    /// for, and the answer is rendered against the COLUMN — never against the
    /// caller's `team_ids`, because the defect ADR-0522 fixes is an owner who
    /// has LEFT the team, whose membership list no longer names it.
    pub fn readable(&self) -> Result<Readable, Status> {
        let reach = owner_reach(&self.setting)?;
        let ladder = self.visible();

        // Three shapes, and the empty one is a shape rather than an omission:
        // a setting that grants nothing must render NO arm, not an arm no row
        // can satisfy — `OR (owner_user_id = ? AND team_id IN ())` is a syntax
        // error for the same reason the TEAM arm above is dropped when the
        // caller belongs to no team.
        let (arm, binds) = if reach.reaches_nothing() {
            (String::new(), Vec::new())
        } else if reach.exceptions().is_empty() {
            (" OR owner_user_id = ?".to_string(), vec![self.user.clone()])
        } else {
            // The exceptions are the teams whose answer DIFFERS from the one
            // every unnamed team gets, so the operator follows that default:
            // where ownership reaches by default the arm SUBTRACTS them, and
            // where it does not the arm is the only thing that grants them.
            // Rendering an inclusion list in both directions would return the
            // complement of the policy in one of them.
            let holes = vec!["?"; reach.exceptions().len()].join(", ");
            let operator = if reach.default_on() { "NOT IN" } else { "IN" };
            let mut binds = vec![self.user.clone()];
            binds.extend(reach.exceptions().iter().cloned());
            (
                format!(" OR (owner_user_id = ? AND team_id {operator} ({holes}))"),
                binds,
            )
        };

        Ok(Readable {
            sql: format!("({ladder}{arm})"),
            binds,
        })
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

/// A read's visibility clause together with the parameters ADR-0522's arm
/// added, which is why they are ONE value rather than two.
///
/// **THE SQL AND ITS BINDINGS CANNOT DRIFT APART BECAUSE NOTHING CAN PRODUCE
/// ONE WITHOUT THE OTHER.** Every other clause in this module renders holes in
/// one method and fills them in another, which works only while both read the
/// same fields — and this arm's hole count depends on a RESOLUTION, so a second
/// method would have to resolve the setting again and agree. A binding that
/// silently disagrees with its statement is how a caller's user id lands in a
/// team column.
pub struct Readable {
    sql: String,
    binds: Vec<String>,
}

impl Readable {
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The parameters ADR-0522's arm added, in the order it rendered them.
    ///
    /// These come LAST: after [`Reach::bind_within`] and [`Reach::bind_visible`],
    /// because the arm is appended to the end of the ladder.
    pub fn bind<'q>(
        &'q self,
        query: Query<'q, MySql, MySqlArguments>,
    ) -> Query<'q, MySql, MySqlArguments> {
        let mut query = query;
        for value in &self.binds {
            query = query.bind(value);
        }
        query
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

#[cfg(test)]
mod tests {
    use super::internal;

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
