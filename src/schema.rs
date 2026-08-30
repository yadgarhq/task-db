//! This module's migrations. `yadgar-store` runs them; it never holds them (D7).

use yadgar_store::migrate::{Migration, MigrationError, MigrationSet};

pub fn migrations() -> Result<MigrationSet, MigrationError> {
    MigrationSet::new(vec![Migration {
        version: 1,
        name: "create_task".into(),
        // `id` is a URN carrying a UUIDv7 (D42) — never the engine's integer key,
        // which stops being portable the moment a module swaps engines (D7).
        //
        // `number` is the semantic reference a human uses. It is per-project, not
        // global: two projects both having task 1 is correct, and the UNIQUE is
        // on the pair.
        //
        // `project_id` is a hierarchical path (D53) matched by PREFIX, so it is
        // indexed for range scans rather than equality alone.
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
    }])
}
