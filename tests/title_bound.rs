//! Migration 7, `task_title_at_most_128`, and the bound it puts in the table
//! (yaadgaar #396).
//!
//! **THE FIRST TEST IS THE ONE THAT DECIDES WHEN THIS SHIPS.** `ADD CONSTRAINT
//! ... CHECK` validates the rows already in the table, so a database holding
//! one title over 128 code points refuses migration 7, and the binary refuses
//! to boot. That is the loud failure chosen on purpose over `MODIFY
//! VARCHAR(128)`, which a non-strict `sql_mode` answers by truncating every
//! long title in place. The cost is the release gate: this migration runs only
//! once `SELECT COUNT(*) FROM task WHERE CHAR_LENGTH(title) > 128` is 0 on the
//! live database — every project, every status, soft-deleted rows included.

mod support;

use tonic::Code;

use support::{World, P_A, U1};
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;
use yadgar_task_db::schema;

/// `n` copies of a TWO-BYTE character. 128 of them is 256 bytes, so a bound in
/// bytes would refuse what the contract admits.
fn persian(n: usize) -> String {
    "ی".repeat(n)
}

/// A row written straight into the table with the title the OLD code allowed.
async fn seed_titled(w: &World, number: u32, title: &str) -> String {
    let id = format!("yadgar:task:pre-title-{number:05}");
    sqlx::query(
        "INSERT INTO task
           (id, project_id, owner_user_id, team_id, visibility, created_by,
            updated_by, number, title, body, status, tags, links)
         VALUES (?, ?, ?, '', 1, ?, ?, ?, ?, 'b', 1, '[]', '[]')",
    )
    .bind(&id)
    .bind(P_A)
    .bind(U1)
    .bind(U1)
    .bind(U1)
    .bind(number)
    .bind(title)
    .execute(&w.pool)
    .await
    .expect("seed a titled row");
    id
}

async fn stored_title(w: &World, id: &str) -> String {
    sqlx::query_scalar("SELECT title FROM task WHERE id = ?")
        .bind(id)
        .fetch_one(&w.pool)
        .await
        .expect("the row")
}

async fn ledger_head(w: &World) -> u64 {
    sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(version), 0) AS UNSIGNED) FROM yadgar_schema_migrations",
    )
    .fetch_one(&w.pool)
    .await
    .expect("the migration ledger")
}

/// **ONE TITLE OF 129 CODE POINTS STOPS THE MIGRATION, AND THE ROW SURVIVES
/// UNCHANGED.** A migration that truncated, or that skipped the row, would
/// pass a test asserting only that `apply` returned.
#[tokio::test]
async fn migration_7_refuses_a_database_holding_a_title_over_128_and_changes_nothing() {
    let w = World::fresh_at("td_title_refuse", 6).await;
    let long = persian(129);
    let id = seed_titled(&w, 1, &long).await;

    let refused = yadgar_store::migrate::apply(
        &w.pool,
        &schema::migrations().expect("set"),
        &support::lock(),
    )
    .await;

    assert!(
        refused.is_err(),
        "a 129-code-point title must stop migration 7, and it applied: {refused:?}"
    );
    assert_eq!(
        stored_title(&w, &id).await,
        long,
        "the row must survive untouched"
    );
    assert_eq!(ledger_head(&w).await, 6, "migration 7 must not be recorded");
}

/// A database whose titles all fit migrates — including a title of exactly
/// 128 two-byte code points, which proves the constraint counts characters.
#[tokio::test]
async fn migration_7_admits_a_database_whose_titles_are_at_most_128_code_points() {
    let w = World::fresh_at("td_title_admit", 6).await;
    let id = seed_titled(&w, 1, &persian(128)).await;

    assert_eq!(w.migrate_to_head().await, 7);
    assert_eq!(stored_title(&w, &id).await, persian(128));
}

/// After migration 7, a create storing 129 code points is INVALID_ARGUMENT
/// rather than an opaque INTERNAL, and nothing is stored. 128 is admitted.
#[tokio::test]
async fn a_create_over_128_code_points_is_invalid_argument_and_stores_nothing() {
    let w = World::fresh("td_title_create").await;

    let status = w
        .try_create(P_A, U1, &"x".repeat(129))
        .await
        .expect_err("the CHECK refuses 129 code points");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert_eq!(
        status.message(),
        "a task title is at most 128 characters (Unicode code points)"
    );
    assert_eq!(w.count_titled(&"x".repeat(129)).await, 0);

    w.try_create(P_A, U1, &persian(128))
        .await
        .expect("128 two-byte code points is within the bound");
}

/// An update storing 129 code points is refused the same way, and the row and
/// its version are left as they were.
#[tokio::test]
async fn an_update_over_128_code_points_is_invalid_argument_and_changes_nothing() {
    let w = World::fresh("td_title_update").await;
    let id = w.create(P_A, U1, "a title that fits").await;

    let status = w
        .edit_as(&w.scope(P_A, U1), &id, &"x".repeat(129))
        .await
        .expect_err("the CHECK refuses 129 code points");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");

    let task = w.read(P_A, U1, &id).await.expect("the task");
    assert_eq!(task.title, "a title that fits");
    assert_eq!(
        task.meta.expect("meta").version,
        1,
        "a refused write moved the version"
    );
}

/// The service under test is the ordinary one: reads are never refused for a
/// title's length. Nothing longer than 128 can exist after migration 7, so this
/// reads a title of exactly 128 through both read paths.
#[tokio::test]
async fn a_title_of_128_code_points_reads_back_whole() {
    let w = World::fresh("td_title_read").await;
    let id = w.create(P_A, U1, &persian(128)).await;

    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").title,
        persian(128)
    );
    let listed =
        w.db.list_tasks(tonic::Request::new(ListTasksRequest {
            scope: w.scope(P_A, U1),
            statuses: vec![],
            page_size: 0,
            page_token: String::new(),
        }))
        .await
        .expect("list")
        .into_inner()
        .tasks;
    assert_eq!(listed[0].title, persian(128));
}
