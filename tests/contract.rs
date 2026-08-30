//! TaskDbService against a real MariaDB.
//!
//! The contract test proto-contract-design.md places in each `-db` repo. It
//! panics rather than skips without an engine, for the reason D69 gives: a suite
//! that quietly passes with nothing behind it is the failure it exists to catch.

use sqlx::Connection;
use tonic::Request;
use yadgar_task_db::pb::yadgar::common::v1::Scope;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;
use yadgar_task_db::{schema, service::TaskDb};

fn dsn() -> String {
    std::env::var("YADGAR_TEST_DSN")
        .expect("YADGAR_TEST_DSN is unset; these tests assert what a real MariaDB does")
}

async fn fresh(db: &str) -> TaskDb {
    let mut root = sqlx::MySqlConnection::connect(&dsn())
        .await
        .expect("connect");
    for stmt in [
        format!("DROP DATABASE IF EXISTS {db}"),
        format!("CREATE DATABASE {db}"),
    ] {
        // AUDIT: `db` is a literal in this file.
        sqlx::raw_sql(sqlx::AssertSqlSafe(stmt))
            .execute(&mut root)
            .await
            .expect("ddl");
    }
    let base = dsn()
        .rsplit_once('/')
        .expect("dsn has a database")
        .0
        .to_string();
    let pool = sqlx::MySqlPool::connect(&format!("{base}/{db}"))
        .await
        .expect("pool");
    yadgar_store::migrate::apply(&pool, &schema::migrations().expect("set"))
        .await
        .expect("migrate");
    TaskDb::new(pool)
}

fn scope(project: &str, user: &str) -> Option<Scope> {
    Some(Scope {
        user_id: user.into(),
        project_id: project.into(),
        team_ids: vec![],
        instance_id: "i-1".into(),
        request_id: "r-1".into(),
    })
}

fn new_task(title: &str) -> Option<Task> {
    Some(Task {
        title: title.into(),
        body: "b".into(),
        status: TaskStatus::Open as i32,
        ..Default::default()
    })
}

#[tokio::test]
async fn numbers_are_per_project_and_start_at_one() {
    let db = fresh("td_number").await;
    for (project, expected) in [("acme/a", 1), ("acme/a", 2), ("acme/b", 1)] {
        let r = db
            .create_task(Request::new(CreateTaskRequest {
                scope: scope(project, "u1"),
                task: new_task("t"),
                idempotency: None,
            }))
            .await
            .expect("create")
            .into_inner();
        assert_eq!(r.number, expected, "number is per-project, not global");
    }
}

/// D53: a project id is a hierarchical path, so an ancestor scope reaches its
/// descendants. Equality matching would return nothing here and look like an
/// empty store rather than a scoping bug.
#[tokio::test]
async fn an_ancestor_scope_sees_descendant_tasks() {
    let db = fresh("td_scope").await;
    db.create_task(Request::new(CreateTaskRequest {
        scope: scope("acme/qwfm/forecast", "u1"),
        task: new_task("deep"),
        idempotency: None,
    }))
    .await
    .expect("create");

    let listed = db
        .list_tasks(Request::new(ListTasksRequest {
            scope: scope("acme/qwfm", "u1"),
            statuses: vec![],
            page_size: 0,
            page_token: String::new(),
        }))
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.tasks.len(), 1, "ancestor must reach the subtree");
}

#[tokio::test]
async fn another_project_cannot_read_the_task() {
    let db = fresh("td_isolate").await;
    let created = db
        .create_task(Request::new(CreateTaskRequest {
            scope: scope("acme/a", "u1"),
            task: new_task("secret"),
            idempotency: None,
        }))
        .await
        .expect("create")
        .into_inner();

    let err = db
        .get_task(Request::new(GetTaskRequest {
            scope: scope("other/z", "u1"),
            key: Some(get_task_request::Key::Id(created.meta.unwrap().id)),
        }))
        .await
        .expect_err("a foreign project must not read this task");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// D8: compare-and-set. A stale writer must be refused, not silently win.
#[tokio::test]
async fn a_stale_version_is_refused() {
    let db = fresh("td_cas").await;
    let id = db
        .create_task(Request::new(CreateTaskRequest {
            scope: scope("acme/a", "u1"),
            task: new_task("t"),
            idempotency: None,
        }))
        .await
        .expect("create")
        .into_inner()
        .meta
        .unwrap()
        .id;

    let upd = |v: u64| UpdateTaskRequest {
        scope: scope("acme/a", "u1"),
        id: id.clone(),
        expect_version: v,
        task: new_task("changed"),
        update_mask: None,
        idempotency: None,
    };

    db.update_task(Request::new(upd(1)))
        .await
        .expect("first update wins");
    let err = db
        .update_task(Request::new(upd(1)))
        .await
        .expect_err("the second writer at version 1 is stale");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

/// D26: delete is owner-only and soft.
#[tokio::test]
async fn only_the_owner_may_delete_and_the_row_survives() {
    let db = fresh("td_delete").await;
    let id = db
        .create_task(Request::new(CreateTaskRequest {
            scope: scope("acme/a", "owner"),
            task: new_task("t"),
            idempotency: None,
        }))
        .await
        .expect("create")
        .into_inner()
        .meta
        .unwrap()
        .id;

    let err = db
        .delete_task(Request::new(DeleteTaskRequest {
            scope: scope("acme/a", "someone-else"),
            id: id.clone(),
            expect_version: 1,
            idempotency: None,
        }))
        .await
        .expect_err("a non-owner must not delete");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    db.delete_task(Request::new(DeleteTaskRequest {
        scope: scope("acme/a", "owner"),
        id: id.clone(),
        expect_version: 1,
        idempotency: None,
    }))
    .await
    .expect("the owner may delete");

    let err = db
        .get_task(Request::new(GetTaskRequest {
            scope: scope("acme/a", "owner"),
            key: Some(get_task_request::Key::Id(id)),
        }))
        .await
        .expect_err("a soft-deleted task is not returned");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// An unset page_size is 0 on the wire, which must not mean "return nothing" —
/// that presents as an empty store rather than as a caller that forgot a field.
#[tokio::test]
async fn an_unset_page_size_returns_a_default_page() {
    let db = fresh("td_page").await;
    for i in 0..3 {
        db.create_task(Request::new(CreateTaskRequest {
            scope: scope("acme/a", "u1"),
            task: new_task(&format!("t{i}")),
            idempotency: None,
        }))
        .await
        .expect("create");
    }
    let listed = db
        .list_tasks(Request::new(ListTasksRequest {
            scope: scope("acme/a", "u1"),
            statuses: vec![],
            page_size: 0,
            page_token: String::new(),
        }))
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.tasks.len(), 3);
}
