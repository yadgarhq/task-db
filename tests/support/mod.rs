//! The shared fixture.
//!
//! **This exists because of what its absence was doing.** Every negative test
//! here needs a SECOND user, and seeding one used to mean repeating the whole
//! database-and-migrate dance inline. So no test seeded one, so no test could
//! catch a scoping bug, so two of them shipped. The structural finding was never
//! "a test is missing" — it was that the missing test was expensive.
//!
//! Each test binary gets its own database, named by the caller, so the suite
//! parallelises without two tests racing on one schema.
#![allow(dead_code)]

use sqlx::{Connection, MySqlPool, Row};
use tonic::{Request, Status};
use yadgar_task_db::pb::yadgar::common::v1::{InheritedSetting, Scope, SettingValue, Visibility};
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;
use yadgar_task_db::{schema, service::TaskDb};

/// Two projects. Siblings, so neither is an ancestor of the other and the
/// subtree axis cannot quietly carry a test that is really about visibility.
pub const P_A: &str = "acme/a";
pub const P_B: &str = "acme/b";

/// Three users, and the third is load-bearing. A TEAM record owned by the user
/// doing the querying would let an accidental `OR owner_user_id = ?` arm make
/// "a non-teammate cannot see it" pass for entirely the wrong reason.
pub const U1: &str = "u1";
pub const U2: &str = "u2";
pub const U3: &str = "u3";

pub const TEAM: &str = "t-platform";
pub const OTHER_TEAM: &str = "t-billing";

/// One `InheritedSetting`, built the way a store would: the organisation's
/// value, its lock, and zero or more team overrides.
///
/// Every ADR-0522 test states its own rather than inheriting one, so the policy
/// under test is readable beside the assertion instead of in this file.
pub fn setting(
    value: SettingValue,
    locked: bool,
    overrides: &[(&str, SettingValue)],
) -> InheritedSetting {
    InheritedSetting {
        org_value: value as i32,
        org_locked: locked,
        team_override: overrides
            .iter()
            .map(|(team, v)| ((*team).to_string(), *v as i32))
            .collect(),
    }
}

/// What `iam-db` migration 12 actually seeds: `SETTING_VALUE_ON`, locked.
///
/// Named so a test asserting the SHIPPED behaviour cannot drift from it by
/// restating the two values and getting one wrong.
pub fn shipped_setting() -> InheritedSetting {
    setting(SettingValue::On, true, &[])
}

/// The D12 ladder WITHOUT ADR-0522's arm, for the tests whose subject is the
/// ladder itself.
///
/// **A TEST THAT ASSERTS AN OWNER REACHES THEIR OWN RECORD MUST STATE THIS.**
/// Under the shipped `ON` the arm reaches every record the caller owns, so such
/// an assertion is answered by ADR-0522 rather than by the rung under test —
/// and a rung nothing exercises is a rung nothing protects.
///
/// MEASURED rather than reasoned: with the fixture default at `ON`, the mutant
/// that renders the PRIVATE rung as `visibility = 1` instead of
/// `visibility NOT IN (2, 3)` survives the WHOLE suite. Stating `OFF` at the
/// call sites that assert an owner's own read is what kills it again. Three
/// other ladder mutants — the rung dropping its owner check, the TEAM arm never
/// rendered, and the ORG arm dropped — die under either default, because each is
/// seen by a caller who does not own the row.
///
/// **THE TEAM RUNG NEEDS THIS TOO, AND FOR THE SAME REASON THE PRIVATE RUNG
/// DOES.** Re-measured 2026-09-06 against `mariadb:11.8.9`: disabling the TEAM
/// arm with its hole count preserved left
/// `the_owner_of_a_team_task_reads_it_through_the_team_arm` — the test named for
/// exactly that rung — GREEN, because under `ON` ADR-0522's blanket arm returned
/// the row instead. That test now states this at a second call site of its own.
///
/// WHICH MUTANT EACH CALL SITE KILLS DIFFERS, so they are not interchangeable:
/// the PRIVATE-rung mutant above dies only at
/// `a_row_with_an_unrecognised_visibility_falls_back_to_private`. NO COUNT IS
/// WRITTEN HERE, and that is the same discipline `World::scope_with` states for
/// the same reason — a count is what went stale last time. It would also be
/// wrong on its own terms: `a_setting_stating_off_leaves_an_owner_outside_the_
/// team_where_they_were` pins this identical arm-free ladder by restating
/// `setting(SettingValue::Off, true, &[])` inline rather than calling this
/// helper, so grepping for the helper under-counts the tests that state the
/// policy.
///
/// This is the same discipline `scope_with` states for the other direction: a
/// test whose subject is the SETTING states its own policy, and so does a test
/// whose subject is the LADDER. Neither inherits one.
pub fn ladder_only() -> InheritedSetting {
    setting(SettingValue::Off, true, &[])
}

/// The pool every fixture opens, and the reason a fixture states one at all.
///
/// **A fixture that says nothing runs at sqlx's default of ten, which is a size
/// production never ships.** `DB_MAX_CONNECTIONS` defaults to eight, D80 made it
/// operator-settable, and `check_engine_headroom` steers a constrained operator
/// DOWNWARDS — so the sizes that matter are small ones, and every engine suite
/// here was running above all of them. A create that took a SECOND connection
/// while holding its first was invisible for exactly that reason: at ten
/// connections and two racers there is always a spare.
///
/// Four rather than eight, because what this pins is a RATIO between concurrency
/// and pool size, and a smaller pool puts the ratio inside a test that finishes.
/// It is still above `store`'s `MIN_CONNECTIONS` of two, so migration keeps the
/// two connections it holds at once.
///
/// A test that is ABOUT the ratio names its own size through
/// [`World::with_pool_size`] rather than leaning on this one — an assertion
/// derived from a default silently stops testing the day the default moves.
const FIXTURE_POOL_SIZE: u32 = 4;

pub fn dsn() -> String {
    std::env::var("YADGAR_TEST_DSN")
        .expect("YADGAR_TEST_DSN is unset; these tests assert what a real MariaDB does")
}

/// The DSN with any trailing `/database` removed.
///
/// Split on the first `/` AFTER the scheme, never the last one anywhere: a
/// rsplit finds the second slash of `mysql://` when the DSN names no database,
/// and silently builds `mysql://<db>` — a URL with the database in the host
/// position, which fails to connect for a reason nothing in the error mentions.
fn base() -> String {
    let dsn = dsn();
    let after_scheme = dsn.find("://").map(|i| i + 3).unwrap_or(0);
    match dsn[after_scheme..].find('/') {
        Some(i) => dsn[..after_scheme + i].to_string(),
        None => dsn,
    }
}

/// A service and the pool behind it, dropped and recreated per test.
///
/// The POOL is exposed deliberately. Visibility has no RPC that sets it — the
/// public API carries no such field, and under D42 the module assigns it — so a
/// TEAM or ORG record can only be seeded at the storage level. Reaching around
/// the service for a fixture is honest; reaching around it in the code under
/// test would not be.
pub struct World {
    pub db: TaskDb,
    pub pool: MySqlPool,
}

impl World {
    pub async fn fresh(name: &str) -> Self {
        Self::build(name, FIXTURE_POOL_SIZE, None, None).await
    }

    /// The same service against a pool of exactly `max_connections`.
    ///
    /// For the tests whose subject IS the pool ceiling. They must name their own
    /// number: an assertion that reads [`FIXTURE_POOL_SIZE`] and spawns that many
    /// racers keeps passing when the default changes, while no longer testing the
    /// ratio it was written for.
    pub async fn with_pool_size(name: &str, max_connections: u32) -> Self {
        Self::build(name, max_connections, None, None).await
    }

    /// A database at migration `version` and no further — the state a data
    /// migration has to be handed in order to be tested at all.
    ///
    /// Every other constructor here builds from the WHOLE set, which is why the
    /// one migration that heals existing rows had no test: the rows it heals
    /// cannot exist in a database the current set created. This stops short, so
    /// a test can write the BEFORE state itself and then migrate over it.
    pub async fn fresh_at(name: &str, version: u64) -> Self {
        Self::build(name, FIXTURE_POOL_SIZE, None, Some(version)).await
    }

    /// The same service against a pool that waits one second for a row lock
    /// instead of the default fifty. Contention is the point of the test that
    /// uses this, and a test that takes fifty seconds to observe it is a test
    /// nobody runs.
    pub async fn impatient(name: &str) -> Self {
        Self::build(name, FIXTURE_POOL_SIZE, Some(1), None).await
    }

    /// Apply whatever is still pending, and answer with the version now
    /// applied.
    ///
    /// The ORDINARY entry point against the FULL set, deliberately: the ledger
    /// says 4, so `apply` selects migration 5 by itself. Handing it a set
    /// containing only the migration under test would exercise a path
    /// production never takes and would prove the test, not the migration.
    pub async fn migrate_to_head(&self) -> u64 {
        yadgar_store::migrate::apply(&self.pool, &schema::migrations().expect("set"), &lock())
            .await
            .expect("migrate")
    }

    async fn build(
        name: &str,
        max_connections: u32,
        lock_wait_secs: Option<u32>,
        upto: Option<u64>,
    ) -> Self {
        let mut root = sqlx::MySqlConnection::connect(&dsn())
            .await
            .expect("connect");
        for stmt in [
            format!("DROP DATABASE IF EXISTS {name}"),
            format!("CREATE DATABASE {name}"),
        ] {
            // AUDIT: `name` is a literal in the calling test file.
            sqlx::raw_sql(sqlx::AssertSqlSafe(stmt))
                .execute(&mut root)
                .await
                .expect("ddl");
        }
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            // STATED, never inherited. See [`FIXTURE_POOL_SIZE`].
            .max_connections(max_connections)
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    if let Some(secs) = lock_wait_secs {
                        sqlx::query("SET SESSION innodb_lock_wait_timeout = ?")
                            .bind(secs)
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(&format!("{}/{name}", base()))
            .await
            .expect("pool");
        let set = match upto {
            Some(version) => schema::migrations_upto(version).expect("set"),
            None => schema::migrations().expect("set"),
        };
        yadgar_store::migrate::apply(&pool, &set, &lock())
            .await
            .expect("migrate");
        Self {
            db: TaskDb::new(pool.clone()),
            pool,
        }
    }

    pub fn scope(&self, project: &str, user: &str) -> Option<Scope> {
        self.scope_in(project, user, &[])
    }

    pub fn scope_in(&self, project: &str, user: &str, teams: &[&str]) -> Option<Scope> {
        self.scope_with(project, user, teams, Some(shipped_setting()))
    }

    /// The same scope carrying a STATED setting of the caller's choosing, for
    /// the tests whose subject is ADR-0522 itself.
    ///
    /// **THE DEFAULT ABOVE IS WHAT THE ESTATE SHIPS: `ON`, LOCKED, the value
    /// `iam-db` migration 12 seeds.** A fixture default that no deployment runs
    /// regression-covers a configuration nobody has, and every test inheriting
    /// it asserts against a policy that exists only here.
    ///
    /// **THE ARGUMENT FOR `OFF` THAT USED TO STAND HERE WAS WRONG ON ITS OWN
    /// EVIDENCE, AND THE CORRECTION IS MEASURED.** It held that the shipped
    /// value would retire the only test that can see the blanket
    /// `OR owner_user_id = ?` arm `Reach::visible` refuses. It does not:
    /// `task-db#30`'s own mutation table records that arm (M4) as killed by
    /// `a_setting_stating_off_leaves_an_owner_outside_the_team_where_they_were`
    /// and by nothing else, and that test states `OFF` at its own call site. The
    /// default it inherits was never what killed M4 — re-measured here, M4 still
    /// dies under this default.
    ///
    /// What the flip DOES cost is the rungs: see [`ladder_only`], which the
    /// tests asserting an owner's own read state. That cost is paid at those
    /// call sites rather than by every test in the suite — and the accounting
    /// was short by one until ledger 475 measured the TEAM rung, so the count is
    /// deliberately not written down here again.
    ///
    /// STATED rather than absent, because absent is now REFUSED: this service
    /// reads the field, so it is in the ENFORCING state and a read carrying no
    /// setting is an `INVALID_ARGUMENT` rather than a pre-enforcement read.
    ///
    /// The literal stays EXHAUSTIVE rather than taking `..Default::default()`.
    /// It is a tripwire: this line is what made the proto bump announce itself,
    /// and spreading a default here would silently absorb the next field a
    /// contract adds — including one a read path must consult.
    pub fn scope_with(
        &self,
        project: &str,
        user: &str,
        teams: &[&str],
        owner_reads_own_record: Option<InheritedSetting>,
    ) -> Option<Scope> {
        Some(Scope {
            user_id: user.into(),
            project_id: project.into(),
            team_ids: teams.iter().map(|t| (*t).to_string()).collect(),
            instance_id: "i-1".into(),
            request_id: "r-1".into(),
            owner_reads_own_record,
        })
    }

    pub async fn try_create(
        &self,
        project: &str,
        user: &str,
        title: &str,
    ) -> Result<CreateTaskResponse, Status> {
        self.db
            .create_task(Request::new(CreateTaskRequest {
                scope: self.scope(project, user),
                task: Some(Task {
                    title: title.into(),
                    body: "b".into(),
                    status: TaskStatus::Open as i32,
                    ..Default::default()
                }),
                idempotency: None,
            }))
            .await
            .map(|r| r.into_inner())
    }

    /// The id of a newly created task, which is what almost every test wants.
    pub async fn create(&self, project: &str, user: &str, title: &str) -> String {
        self.try_create(project, user, title)
            .await
            .expect("create")
            .meta
            .expect("meta")
            .id
    }

    pub async fn read(&self, project: &str, user: &str, id: &str) -> Result<Task, Status> {
        self.read_as(&self.scope(project, user), id).await
    }

    pub async fn read_as(&self, scope: &Option<Scope>, id: &str) -> Result<Task, Status> {
        self.db
            .get_task(Request::new(GetTaskRequest {
                scope: scope.clone(),
                key: Some(get_task_request::Key::Id(id.into())),
            }))
            .await
            .map(|r| r.into_inner().task.expect("task"))
    }

    pub async fn list_as(&self, scope: &Option<Scope>) -> Vec<Task> {
        self.db
            .list_tasks(Request::new(ListTasksRequest {
                scope: scope.clone(),
                statuses: vec![],
                page_size: 0,
                page_token: String::new(),
            }))
            .await
            .expect("list")
            .into_inner()
            .tasks
    }

    pub async fn list(&self, project: &str, user: &str) -> Vec<Task> {
        self.list_as(&self.scope(project, user)).await
    }

    pub async fn edit_as(
        &self,
        scope: &Option<Scope>,
        id: &str,
        title: &str,
    ) -> Result<UpdateTaskResponse, Status> {
        self.db
            .update_task(Request::new(UpdateTaskRequest {
                scope: scope.clone(),
                id: id.into(),
                expect_version: 1,
                task: Some(Task {
                    title: title.into(),
                    body: "b".into(),
                    status: TaskStatus::Open as i32,
                    ..Default::default()
                }),
                update_mask: None,
                idempotency: None,
            }))
            .await
            .map(|r| r.into_inner())
    }

    /// Promote a record past PRIVATE. There is no RPC for this on purpose (D42),
    /// so the fixture writes the column directly.
    pub async fn promote(&self, id: &str, visibility: Visibility, team_id: &str) {
        self.set_visibility(id, visibility as i8, team_id).await
    }

    /// The same thing without the enum, for the values the enum forbids and the
    /// old code wrote anyway.
    pub async fn set_visibility(&self, id: &str, visibility: i8, team_id: &str) {
        sqlx::query("UPDATE task SET visibility = ?, team_id = ? WHERE id = ?")
            .bind(visibility)
            .bind(team_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .expect("promote");
    }

    /// What is actually in the column — the only way to tell "assigned PRIVATE"
    /// from "took the caller's word and happened to agree".
    pub async fn stored_visibility(&self, id: &str) -> i8 {
        sqlx::query("SELECT visibility FROM task WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .expect("select")
            .try_get("visibility")
            .expect("visibility")
    }

    pub async fn stored_team(&self, id: &str) -> String {
        sqlx::query("SELECT team_id FROM task WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .expect("select")
            .try_get("team_id")
            .expect("team_id")
    }

    /// The bytes actually in `task_write.request_fingerprint` for one claim.
    ///
    /// Read from the COLUMN rather than recomputed, because the storage encoding
    /// is part of what a known-answer test pins: raw bytes against hex, and a
    /// digest of some other width silently padded out by `BINARY(32)`, are both
    /// invisible to an assertion that stops at the function's return value.
    ///
    /// `Option` because the column is nullable — a claim recorded before
    /// migration 6 carries no fingerprint, and absent is not empty.
    pub async fn stored_fingerprint(
        &self,
        project: &str,
        user: &str,
        key: &str,
    ) -> Option<Vec<u8>> {
        sqlx::query_scalar(
            "SELECT request_fingerprint FROM task_write
              WHERE project_id = ? AND user_id = ? AND idem_key = ?",
        )
        .bind(project)
        .bind(user)
        .bind(key)
        .fetch_one(&self.pool)
        .await
        .expect("select request_fingerprint")
    }

    pub async fn count_titled(&self, title: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM task WHERE title = ?")
            .bind(title)
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }

    /// One row written straight into the table, with `visibility` as a raw
    /// integer — including the values the enum forbids and the old code wrote
    /// anyway. Answers with the id.
    ///
    /// Not created through the service and then corrected: the service assigns
    /// visibility itself (D42), so routing an OLD row through TODAY's writer
    /// would make the fixture depend on the very behaviour the migration exists
    /// to compensate for. The row a pre-migration database holds is written the
    /// way that database's code wrote it.
    pub async fn seed_row(
        &self,
        project: &str,
        user: &str,
        number: u32,
        visibility: i8,
        team_id: &str,
    ) -> String {
        let id = format!("yadgar:task:pre-{project}-{number:05}");
        sqlx::query(
            "INSERT INTO task
               (id, project_id, owner_user_id, team_id, visibility, created_by,
                updated_by, number, title, body, status, tags, links)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'b', ?, '[]', '[]')",
        )
        .bind(&id)
        .bind(project)
        .bind(user)
        .bind(team_id)
        .bind(visibility)
        .bind(user)
        .bind(user)
        .bind(number)
        .bind(format!("seeded {number}"))
        .bind(TaskStatus::Open as i8)
        .execute(&self.pool)
        .await
        .expect("seed row");
        id
    }

    /// Rows inserted straight into the table, for the tests that need more of
    /// them than an RPC per row is worth.
    pub async fn seed_rows(&self, project: &str, user: &str, count: u32, status: TaskStatus) {
        for n in 1..=count {
            sqlx::query(
                "INSERT INTO task
                   (id, project_id, owner_user_id, team_id, visibility, created_by,
                    updated_by, number, title, body, status, tags, links)
                 VALUES (?, ?, ?, '', ?, ?, ?, ?, ?, 'b', ?, '[]', '[]')",
            )
            .bind(format!("yadgar:task:seed-{project}-{n:05}"))
            .bind(project)
            .bind(user)
            .bind(Visibility::Org as i8)
            .bind(user)
            .bind(user)
            .bind(n)
            .bind(format!("seeded {n}"))
            .bind(status as i8)
            .execute(&self.pool)
            .await
            .expect("seed");
        }
        sqlx::query(
            "INSERT INTO task_counter (project_id, next_number) VALUES (?, ?)
             ON DUPLICATE KEY UPDATE next_number = GREATEST(next_number, VALUES(next_number))",
        )
        .bind(project)
        .bind(count)
        .execute(&self.pool)
        .await
        .expect("counter");
    }
}

/// The cast every scoping test needs, seeded once.
///
/// Both PRIVATE records sit in the SAME project, which is the point: if they sat
/// in different projects the subtree predicate would hide one of them and the
/// test would pass with the visibility ladder still unimplemented.
pub struct Cast {
    pub world: World,
    /// PRIVATE, owned by U1, in P_A.
    pub u1_private: String,
    /// PRIVATE, owned by U2, in P_A.
    pub u2_private: String,
    /// TEAM `TEAM`, owned by U3, in P_A.
    pub u3_team: String,
    /// ORG, owned by U3, in P_A.
    pub u3_org: String,
    /// PRIVATE, owned by U1, in the sibling project P_B.
    pub u1_elsewhere: String,
}

impl std::ops::Deref for Cast {
    type Target = World;
    fn deref(&self) -> &World {
        &self.world
    }
}

pub async fn two_projects_two_users(name: &str) -> Cast {
    let world = World::fresh(name).await;

    let u1_private = world.create(P_A, U1, "u1 private").await;
    let u2_private = world.create(P_A, U2, "u2 private").await;
    let u3_team = world.create(P_A, U3, "u3 team").await;
    let u3_org = world.create(P_A, U3, "u3 org").await;
    let u1_elsewhere = world.create(P_B, U1, "u1 elsewhere").await;

    world.promote(&u3_team, Visibility::Team, TEAM).await;
    world.promote(&u3_org, Visibility::Org, "").await;

    Cast {
        world,
        u1_private,
        u2_private,
        u3_team,
        u3_org,
        u1_elsewhere,
    }
}

/// The migration lock as `main` builds it, with the chart's shipped wait.
/// Stated in the TEST because `yadgar-store` has no default for it any more
/// (ADR-0569, ledger 814).
fn lock() -> yadgar_store::migrate::LockOptions {
    yadgar_store::migrate::LockOptions::new(60).expect("60 seconds is a wait")
}

// ══════════════════════════════════════════════════════════════════════════
// RUNNING THE REAL BINARY (ledger 748, `tests/exit_chain.rs`). Shaped to match
// `yadgarhq/project-db#54`'s own copy of this harness rather than
// `yadgarhq/task`'s — project-db is the sibling `-db` twin whose
// `rotate::watch_set` is the SAME shape this repository's is (listener TLS,
// the database credential, `database.sslCaSecret`'s authority, and the
// mounted schedule document), and its harness is the one already measured
// green against both exit_chain cases in CI. `task` dials no engine before it
// listens, which is NOT true here — see `BOOT_DEADLINE`.
//
// THE MOUNT NAMESPACE IS STILL NEEDED: this binary's own `rotate::
// Configuration::mounted()` is `yadgar_lifecycle::rotate::Configuration::
// mounted`, the same function `task` and `project-db` both call, and it reads
// `/etc/yadgar/config/shared/shared.yaml` unconditionally with no environment
// override. A test cannot write under the host's `/etc`, so every run goes
// through `unshare -rm`, which gives the binary a private mount table in
// which `/etc` is an overlay over the real one, with the document's
// directory bind-mounted into it — the same shape kubelet gives a ConfigMap
// volume, and the only one that lets a test rewrite the file from OUTSIDE the
// namespace afterwards.
//
// NO PROBE, NO SKIP. If the runner forbids unprivileged user namespaces,
// `mount` fails, the script exits non-zero before the binary starts, and
// every wait below reports that with the captured stderr.
// ══════════════════════════════════════════════════════════════════════════

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The binary under test.
pub const BIN: &str = env!("CARGO_BIN_EXE_yadgar-task-db");

/// The rotation schedule every run is given: poll each second, no splay. A
/// rewrite is therefore noticed within one second and acted on at once.
pub const FIXTURE: &str = "tlsRotation:\n  pollSeconds: 1\n  splayMaxSeconds: 0\n";

/// The poll and splay in [`FIXTURE`], for deadlines derived from them.
pub const POLL: Duration = Duration::from_secs(1);
pub const SPLAY_MAX: Duration = Duration::from_secs(0);

/// What every deadline adds on top of the time the binary is allowed.
/// Generous, because a shared CI runner is slow, a probe and a migration are
/// both real round trips to the engine, and the waits it bounds are seconds
/// long regardless.
pub const MARGIN: Duration = Duration::from_secs(10);

/// How long a boot may take to reach its "listening" line. Longer than
/// `task`'s own: this binary probes AND migrates a real engine before it
/// opens a socket, neither of which a passthrough service does.
pub const BOOT_DEADLINE: Duration = Duration::from_secs(30);

/// The script `unshare` runs. Paths arrive as positional arguments — `$1` the
/// per-test root, `$2` the binary — rather than interpolated into the text.
const SCRIPT: &str = r#"set -e
mount -t overlay overlay -o "lowerdir=/etc,upperdir=$1/upper,workdir=$1/work" /etc
mkdir -p /etc/yadgar/config/shared
mount --bind "$1/shared" /etc/yadgar/config/shared
exec "$2""#;

/// `user, password, host, port` out of `mysql://user:password@host:port[/db]`
/// — [`dsn`]'s own format, stated here rather than imported because nothing
/// in this crate parses a DSN. PURE.
///
/// NEVER `{dsn}` IN A PANIC MESSAGE: it carries the credential
/// `YADGAR_TEST_DSN` names, and a panic message is exactly the kind of text
/// that ends up in a captured test log. Every message below names the SHAPE
/// that is missing and never the value that failed to produce it.
fn parse_dsn(dsn: &str) -> (String, String, String, u16) {
    let rest = dsn
        .strip_prefix("mysql://")
        .expect("YADGAR_TEST_DSN must start with mysql://");
    let (creds, host_port) = rest
        .split_once('@')
        .expect("YADGAR_TEST_DSN must carry user:password@host:port");
    let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
    let host_port = host_port.split('/').next().unwrap_or(host_port);
    let (host, port) = host_port
        .split_once(':')
        .expect("YADGAR_TEST_DSN must carry host:port");
    (
        user.to_string(),
        password.to_string(),
        host.to_string(),
        port.parse()
            .expect("YADGAR_TEST_DSN's port must be a number"),
    )
}

/// A throwaway database this process creates and the SPAWNED BINARY migrates
/// into — `World::build`'s own DDL, without opening a pool here: the binary
/// owns that pool, not this test.
pub async fn fresh_boot_database(name: &str) {
    let mut root = sqlx::MySqlConnection::connect(&dsn())
        .await
        .expect("connect to create the boot fixture's database");
    for stmt in [
        format!("DROP DATABASE IF EXISTS {name}"),
        format!("CREATE DATABASE {name}"),
    ] {
        // AUDIT: `name` is a literal at every call site in this test target.
        sqlx::raw_sql(sqlx::AssertSqlSafe(stmt))
            .execute(&mut root)
            .await
            .expect("ddl");
    }
}

/// The environment the chart's `deployment.yaml` renders for a CLEARTEXT
/// deployment against a real engine, with loopback listeners.
///
/// Port 0 on both listeners, because the cases in a target run in parallel.
/// `DB_HOST`/`DB_PORT`/`DB_USER` and the password come straight out of
/// [`YADGAR_TEST_DSN`][dsn] — the same engine every other integration test in
/// this crate already migrates against — so this harness introduces no
/// second source for "which database is real". `password_file` is written by
/// the caller and handed back here only as a path: `CredentialSource::
/// SecretFile` reads it directly and has no mount requirement of its own,
/// unlike the rotation document.
pub fn boot_cleartext_env(db_name: &str, password_file: &Path) -> Vec<(String, String)> {
    let (user, password, host, port) = parse_dsn(&dsn());
    std::fs::write(password_file, &password).expect("the boot fixture's password file");
    vec![
        ("DB_HOST".to_string(), host),
        ("DB_PORT".to_string(), port.to_string()),
        ("DB_NAME".to_string(), db_name.to_string()),
        ("DB_USER".to_string(), user),
        (
            "DB_PASSWORD_FILE".to_string(),
            password_file.display().to_string(),
        ),
        // SMALL ON PURPOSE: this harness opens one pool against one throwaway
        // database, never the sizes a chart renders for a real deployment.
        ("DB_MAX_CONNECTIONS".to_string(), "4".to_string()),
        ("REPLICAS".to_string(), "1".to_string()),
        ("DB_ENGINE_MAX_CONNECTIONS".to_string(), "200".to_string()),
        // The fixture engine speaks no TLS; `disabled` is `parse_ssl_mode`'s
        // own word for that, distinct from the listener's `LISTEN_TLS_ENABLED`
        // below.
        ("DB_SSL_MODE".to_string(), "disabled".to_string()),
        (
            "DB_MIGRATION_LOCK_TIMEOUT_SECONDS".to_string(),
            "60".to_string(),
        ),
        ("LISTEN".to_string(), "127.0.0.1:0".to_string()),
        ("METRICS_LISTEN".to_string(), "127.0.0.1:0".to_string()),
        ("LISTEN_TLS_ENABLED".to_string(), "0".to_string()),
    ]
}

/// One line the binary wrote, and which stream it came from.
enum Line {
    Out(String),
    Err(String),
}

/// The binary, running in its own mount namespace, with its output pumped.
pub struct Booted {
    child: Child,
    lines: Receiver<Line>,
    seen: Vec<String>,
    root: PathBuf,
}

impl Booted {
    /// Start the binary with exactly `vars` (and `PATH`) in its environment.
    pub fn start(vars: &[(String, String)]) -> Self {
        let root = fresh_root();
        // `unshare`, `mount` and `sh` are found on the caller's PATH; on some
        // hosts none of them is under /usr/bin. A rig with no PATH fails here
        // rather than guessing one.
        let path = std::env::var_os("PATH").expect("the test runner must have a PATH");
        let mut child = Command::new("unshare")
            .args(["-rm", "sh", "-c", SCRIPT, "sh"])
            .arg(&root)
            .arg(BIN)
            .env_clear()
            .env("PATH", path)
            .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("`unshare` could not be started: {e}"));

        // ONE THREAD PER STREAM, pumping for the life of the child. Reading
        // only until the "listening" line would leave the pipe to fill and
        // the binary to block on its next log line.
        let (tx, lines) = mpsc::channel();
        let out = child.stdout.take().expect("stdout was piped");
        let err = child.stderr.take().expect("stderr was piped");
        let tx_err = tx.clone();
        std::thread::spawn(move || pump(out, move |l| tx.send(Line::Out(l)).is_ok()));
        std::thread::spawn(move || pump(err, move |l| tx_err.send(Line::Err(l)).is_ok()));

        Self {
            child,
            lines,
            seen: Vec::new(),
            root,
        }
    }

    /// The binary's pid. `exec` all the way down makes it the direct child.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait for a line satisfying `matches`, or fail on `deadline`.
    ///
    /// A child whose output ends first — it exited, or the mount namespace was
    /// refused and it never started — fails at once with what it printed,
    /// rather than as a timeout.
    pub fn wait_for_line(
        &mut self,
        what: &str,
        deadline: Duration,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let until = Instant::now() + deadline;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(Line::Out(l) | Line::Err(l)) => {
                    self.seen.push(l.clone());
                    if matches(&l) {
                        return l;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.fail(&format!("waited {deadline:?} for {what}; it never came"))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self.reap(Duration::from_secs(5));
                    self.fail(&format!(
                        "the binary's output ended before {what} ({})",
                        describe(status)
                    ))
                }
            }
        }
    }

    /// Wait for the boot to reach the line `main` logs once the signal
    /// handlers are armed and the server is spawned.
    pub fn wait_until_listening(&mut self) {
        self.wait_for_line("the \"task-db listening\" line", BOOT_DEADLINE, |l| {
            l.contains("\"task-db listening\"")
        });
    }

    /// Send SIGTERM, the way kubelet ends a pod.
    pub fn terminate(&mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.pid().to_string()])
            .status()
            .unwrap_or_else(|e| panic!("`kill` could not be started: {e}"));
        if !status.success() {
            self.fail(&format!("`kill -TERM` failed: {status}"));
        }
    }

    /// Wait for the process to exit, or fail on `deadline`.
    pub fn wait_for_exit(&mut self, what: &str, deadline: Duration) -> ExitStatus {
        match self.reap(deadline) {
            Some(status) => {
                self.drain_lines();
                status
            }
            None => self.fail(&format!(
                "waited {deadline:?} for the process to exit {what}; it was still running"
            )),
        }
    }

    /// Replace the mounted `shared.yaml` the way kubelet replaces a projected
    /// file: write a sibling, then rename it over the original.
    pub fn rewrite_shared(&self, contents: &str) {
        let dir = self.root.join("shared");
        let staged = dir.join(".shared.yaml.next");
        std::fs::write(&staged, contents).expect("the staged document must be written");
        std::fs::rename(&staged, dir.join("shared.yaml"))
            .expect("the staged document must replace the mounted one");
    }

    /// Every line the binary has written so far.
    pub fn seen(&self) -> &[String] {
        &self.seen
    }

    /// Fail the test: kill the child, then panic with everything it printed.
    pub fn fail(&mut self, why: &str) -> ! {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.drain_lines();
        panic!(
            "{why}\n--- everything the binary wrote ---\n{}",
            self.seen.join("\n")
        );
    }

    fn reap(&mut self, deadline: Duration) -> Option<ExitStatus> {
        let until = Instant::now() + deadline;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() >= until => return None,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("the child could not be waited on: {e}"),
            }
        }
    }

    fn drain_lines(&mut self) {
        // The pumps end at EOF, which follows the exit closely; a short bound
        // keeps a stray grandchild holding the pipe from hanging the test.
        while let Ok(Line::Out(l) | Line::Err(l)) =
            self.lines.recv_timeout(Duration::from_millis(500))
        {
            self.seen.push(l);
        }
    }
}

impl Drop for Booted {
    fn drop(&mut self) {
        // A panicking test must not leave a server running.
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_root(&self.root);
    }
}

/// The exit status as a sentence that says whether a signal ended it.
pub fn describe(status: Option<ExitStatus>) -> String {
    match status {
        None => "still running".to_string(),
        Some(s) => format!("code {:?}, signal {:?}", s.code(), s.signal()),
    }
}

/// A per-test directory: the overlay's upper and work dirs, and the directory
/// bind-mounted as `/etc/yadgar/config/shared`.
fn fresh_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "yadgar-task-db-exit-chain-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    for dir in ["upper", "work", "shared"] {
        std::fs::create_dir_all(root.join(dir)).expect("the test root must be created");
    }
    std::fs::write(root.join("shared").join("shared.yaml"), FIXTURE)
        .expect("the fixture document must be written");
    root
}

/// Best effort. Overlayfs leaves `work/work` with mode 0, so it is opened up
/// before the tree is removed.
fn remove_root(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(
        root.join("work").join("work"),
        std::fs::Permissions::from_mode(0o700),
    );
    let _ = std::fs::remove_dir_all(root);
}

fn pump(stream: impl Read, mut send: impl FnMut(String) -> bool) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if !send(line) {
            return;
        }
    }
}
