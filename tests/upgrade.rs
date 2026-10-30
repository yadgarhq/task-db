//! A store an OLDER release migrated, meeting this release's image (yaadgaar
//! #245). Ported from project-db's `tests/upgrade.rs`.
//!
//! **EVERY OTHER SUITE HERE STARTS FROM AN EMPTY DATABASE** (or from a prefix of
//! TODAY's set, `World::fresh_at`), so it can only see the schema today's SQL
//! builds. A migration edited in place after it shipped reaches no store that
//! already applied it: the ledger records the version, never the SQL.
//! project-db v0.7.0 shipped broken that way. This suite builds a store the way
//! the oldest release did, migrates it to this release, and requires the result
//! to match a fresh install.
//!
//! [`V0_1`] is v0.1.0's migration set, frozen VERBATIM. It is a literal, never
//! read from git at run time: it is history, and history does not change. A tag
//! by tag audit found no migration here edited after its first release, so
//! today this suite passes; it turns red the day migration 1 is edited in place.

mod support;

use support::{lock, World};
use yadgar_store::migrate::{Migration, MigrationSet};

/// v0.1.0's migration 1, SQL byte-for-byte as that tag carries it.
#[allow(non_snake_case)]
fn V0_1() -> Vec<Migration> {
    vec![Migration {
        version: 1,
        name: "create_task".into(),
        sql: "CREATE TABLE task (
                  id             VARCHAR(96)  NOT NULL PRIMARY KEY,
                  version        BIGINT UNSIGNED NOT NULL DEFAULT 1,
                  project_id     VARCHAR(255) NOT NULL,
                  owner_user_id  VARCHAR(64)  NOT NULL,
                  team_id        VARCHAR(64)  NOT NULL DEFAULT '',
                  visibility     TINYINT      NOT NULL,
                  created_by     VARCHAR(64)  NOT NULL,
                  updated_by     VARCHAR(64)  NOT NULL,
                  created_at     TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
                  updated_at     TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP
                                              ON UPDATE CURRENT_TIMESTAMP,
                  deleted_at     TIMESTAMP    NULL DEFAULT NULL,
                  number         INT UNSIGNED NOT NULL,
                  title          VARCHAR(512) NOT NULL,
                  body           MEDIUMTEXT   NOT NULL,
                  status         TINYINT      NOT NULL,
                  UNIQUE KEY uq_task_project_number (project_id, number),
                  KEY ix_task_project_status (project_id, status),
                  KEY ix_task_project_created (project_id, created_at)
              ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
            .into(),
    }]
}

/// Every table, column, CHECK, index and foreign key a fresh install has, an
/// upgraded store has — with the same type, nullability, default, collation,
/// position and referential rules. A query is only right against the schema it
/// meets.
#[tokio::test]
async fn an_upgraded_store_has_exactly_the_schema_a_fresh_one_has() {
    let fresh = World::fresh("task_db_upgrade_parity_fresh").await;

    // Nothing of today's set, then v0.1.0's, then today's over the top.
    let upgraded = World::fresh_at("task_db_upgrade_parity_old", 0).await;
    let history = MigrationSet::new(V0_1()).expect("v0.1.0's set");
    yadgar_store::migrate::apply(&upgraded.pool, &history, &lock())
        .await
        .expect("migrate the history");
    upgraded.migrate_to_head().await;

    assert_eq!(
        shape(&upgraded.pool, "task_db_upgrade_parity_old").await,
        shape(&fresh.pool, "task_db_upgrade_parity_fresh").await
    );
}

/// `(table, column, type, nullable, default, collation, position)`.
type Column = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    u64,
);

/// `(table, constraint, column, referenced table, referenced column, on update,
/// on delete)`.
type ForeignKey = (String, String, String, String, String, String, String);

type Shape = (
    Vec<(String, Option<String>, Option<String>)>,
    Vec<Column>,
    Vec<(String, String, String)>,
    Vec<(String, String, String, u64, i64)>,
    Vec<ForeignKey>,
);

/// `information_schema`'s view of one database: tables, columns, CHECKs,
/// indexes and foreign keys. This schema has no foreign key today; comparing
/// them costs nothing and covers the first one added.
async fn shape(pool: &sqlx::MySqlPool, db: &str) -> Shape {
    let tables = sqlx::query_as(
        "SELECT TABLE_NAME, ENGINE, TABLE_COLLATION
           FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?
          ORDER BY TABLE_NAME",
    )
    .bind(db)
    .fetch_all(pool)
    .await
    .expect("tables");
    let columns = sqlx::query_as(
        "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT,
                COLLATION_NAME, CAST(ORDINAL_POSITION AS UNSIGNED)
           FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ?
          ORDER BY TABLE_NAME, ORDINAL_POSITION",
    )
    .bind(db)
    .fetch_all(pool)
    .await
    .expect("columns");
    let checks = sqlx::query_as(
        "SELECT TABLE_NAME, CONSTRAINT_NAME, CHECK_CLAUSE
           FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = ?
          ORDER BY TABLE_NAME, CONSTRAINT_NAME",
    )
    .bind(db)
    .fetch_all(pool)
    .await
    .expect("checks");
    let indexes = sqlx::query_as(
        "SELECT TABLE_NAME, INDEX_NAME, COLUMN_NAME,
                CAST(SEQ_IN_INDEX AS UNSIGNED), CAST(NON_UNIQUE AS SIGNED)
           FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ?
          ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(db)
    .fetch_all(pool)
    .await
    .expect("indexes");
    let foreign_keys = sqlx::query_as(
        "SELECT r.TABLE_NAME, r.CONSTRAINT_NAME, k.COLUMN_NAME,
                r.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME,
                r.UPDATE_RULE, r.DELETE_RULE
           FROM information_schema.REFERENTIAL_CONSTRAINTS r
           JOIN information_schema.KEY_COLUMN_USAGE k
             ON k.CONSTRAINT_SCHEMA = r.CONSTRAINT_SCHEMA
            AND k.TABLE_NAME = r.TABLE_NAME
            AND k.CONSTRAINT_NAME = r.CONSTRAINT_NAME
          WHERE r.CONSTRAINT_SCHEMA = ?
          ORDER BY r.TABLE_NAME, r.CONSTRAINT_NAME, k.ORDINAL_POSITION",
    )
    .bind(db)
    .fetch_all(pool)
    .await
    .expect("foreign keys");
    (tables, columns, checks, indexes, foreign_keys)
}
