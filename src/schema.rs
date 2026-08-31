//! This module's migrations. `yadgar-store` runs them; it never holds them (D7).
//!
//! Migrations are APPENDED, never edited. Every one of these has already run
//! against a live database, and a store that has applied version 1 will never
//! apply it again — so a correction to an old migration is a correction only new
//! installations receive, which is the worst of both.

use yadgar_store::migrate::{Migration, MigrationError, MigrationSet};

pub fn migrations() -> Result<MigrationSet, MigrationError> {
    MigrationSet::new(vec![
        Migration {
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
        },
        Migration {
            version: 2,
            name: "task_tags_and_links".into(),
            // The contract carried both from its first tag and the store had
            // neither, so `row_to_task` returned an empty vec for each and a
            // caller's tags vanished without a word.
            //
            // JSON rather than a child table, deliberately: nothing queries BY a
            // tag today, so a join per read would cost an extra statement and an
            // ordering problem to serve no query. The day a tag filter appears
            // this becomes a child table, and that is a migration. MariaDB's
            // JSON is LONGTEXT with a json_valid CHECK, so a DEFAULT is
            // permitted here where MySQL would refuse it.
            sql: "ALTER TABLE task
                    ADD COLUMN tags  JSON NOT NULL DEFAULT '[]',
                    ADD COLUMN links JSON NOT NULL DEFAULT '[]'"
                .into(),
        },
        Migration {
            version: 3,
            name: "task_number_counter".into(),
            // `SELECT MAX(number) + 1 ... FOR UPDATE` locks the rows it reads,
            // and an empty project has none — so the statement meant to
            // serialise allocation took no lock at all in precisely the case
            // that needed one. Two concurrent creates in a new project both read
            // 1, and the UNIQUE then turned one of them into a failure.
            //
            // A counter ROW always exists to be locked, which is the whole
            // difference. `next_number` is the last number HANDED OUT, so a
            // fresh project starts at 0 and its first allocation is 1.
            //
            // The backfill does NOT filter on deleted_at: a deleted task keeps
            // its number, and reissuing one would point an old reference at a
            // different task.
            sql: "CREATE TABLE task_counter (
                      project_id  VARCHAR(255) NOT NULL PRIMARY KEY,
                      next_number INT UNSIGNED NOT NULL
                  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
                  INSERT INTO task_counter (project_id, next_number)
                       SELECT project_id, MAX(number) FROM task GROUP BY project_id"
                .into(),
        },
        Migration {
            version: 4,
            name: "task_write_idempotency".into(),
            // D9: a repeated key is a replay, so the ORIGINAL outcome has to
            // still exist to be returned. `response` is the encoded response
            // message and `rpc` names which one it is — decoding a stored
            // CreateTaskResponse as a DeleteTaskResponse would succeed and mean
            // nothing.
            //
            // Keyed on (project, user, key) rather than on the key alone: the
            // key is CLIENT-supplied, so two clients will eventually choose the
            // same string, and deduplicating across users would hand one of them
            // the other's record.
            sql: "CREATE TABLE task_write (
                      project_id VARCHAR(255)    NOT NULL,
                      user_id    VARCHAR(64)     NOT NULL,
                      idem_key   VARCHAR(255)    NOT NULL,
                      rpc        VARCHAR(32)     NOT NULL,
                      response   VARBINARY(4096) NOT NULL,
                      created_at TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
                      PRIMARY KEY (project_id, user_id, idem_key)
                  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
                .into(),
        },
    ])
}
