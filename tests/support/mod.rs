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
use yadgar_task_db::pb::yadgar::common::v1::{Scope, Visibility};
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
        Self::build(name, None).await
    }

    /// The same service against a pool that waits one second for a row lock
    /// instead of the default fifty. Contention is the point of the test that
    /// uses this, and a test that takes fifty seconds to observe it is a test
    /// nobody runs.
    pub async fn impatient(name: &str) -> Self {
        Self::build(name, Some(1)).await
    }

    async fn build(name: &str, lock_wait_secs: Option<u32>) -> Self {
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
        yadgar_store::migrate::apply(&pool, &schema::migrations().expect("set"))
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
        Some(Scope {
            user_id: user.into(),
            project_id: project.into(),
            team_ids: teams.iter().map(|t| (*t).to_string()).collect(),
            instance_id: "i-1".into(),
            request_id: "r-1".into(),
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
        sqlx::query("UPDATE task SET visibility = ?, team_id = ? WHERE id = ?")
            .bind(visibility as i8)
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

    pub async fn count_titled(&self, title: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM task WHERE title = ?")
            .bind(title)
            .fetch_one(&self.pool)
            .await
            .expect("count")
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
