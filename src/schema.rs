//! This module's migrations. `yadgar-store` runs them; it never holds them (D7).
//!
//! Migrations are APPENDED, never edited. Every one of these has already run
//! against a live database, and a store that has applied version 1 will never
//! apply it again — so a correction to an old migration is a correction only new
//! installations receive, which is the worst of both.
//!
//! One function each rather than one list of literals: a migration is an
//! independent, immutable unit, and giving each a name puts its reasoning next
//! to its SQL instead of in a comment halfway down a table.

use yadgar_store::migrate::{Migration, MigrationError, MigrationSet};

pub fn migrations() -> Result<MigrationSet, MigrationError> {
    MigrationSet::new(all())
}

/// The same set, truncated at `version`.
///
/// **This exists because a data migration is untestable without it.** Migration
/// 5 heals rows that a database created today cannot contain: a fixture that
/// only ever builds from the whole set has no way to present the BEFORE state,
/// so the one migration that touches existing rows was the one migration no
/// test could reach. Truncating the set is the seam — a test builds a database
/// at 4, writes the rows the old code wrote, and then runs the ordinary
/// `apply` against the full set, which is the production path rather than a
/// second one.
///
/// Nothing in the service calls this; the service always migrates to head.
pub fn migrations_upto(version: u64) -> Result<MigrationSet, MigrationError> {
    MigrationSet::new(all().into_iter().filter(|m| m.version <= version).collect())
}

/// One list, in one place, so `migrations` and `migrations_upto` cannot drift
/// into disagreeing about what the set is.
fn all() -> Vec<Migration> {
    vec![
        create_task(),
        task_tags_and_links(),
        task_number_counter(),
        task_write_idempotency(),
        heal_unspecified_visibility(),
        task_write_request_fingerprint(),
    ]
}

/// `id` is a URN carrying a UUIDv7 (D42) — never the engine's integer key, which
/// stops being portable the moment a module swaps engines (D7).
///
/// `number` is the semantic reference a human uses. It is per-project, not
/// global: two projects both having task 1 is correct, and the UNIQUE is on the
/// pair.
///
/// `project_id` is a hierarchical path (D53) matched by PREFIX, so it is indexed
/// for range scans rather than equality alone.
fn create_task() -> Migration {
    Migration {
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
    }
}

/// The contract carried both from its first tag and the store had neither, so
/// `row_to_task` returned an empty vec for each and a caller's tags vanished
/// without a word.
///
/// JSON rather than a child table, deliberately: nothing queries BY a tag today,
/// so a join per read would cost an extra statement and an ordering problem to
/// serve no query. The day a tag filter appears this becomes a child table, and
/// that is a migration. MariaDB's JSON is LONGTEXT with a json_valid CHECK, so a
/// DEFAULT is permitted here where MySQL would refuse it.
fn task_tags_and_links() -> Migration {
    Migration {
        version: 2,
        name: "task_tags_and_links".into(),
        sql: "ALTER TABLE task
                ADD COLUMN tags  JSON NOT NULL DEFAULT '[]',
                ADD COLUMN links JSON NOT NULL DEFAULT '[]'"
            .into(),
    }
}

/// `SELECT MAX(number) + 1 ... FOR UPDATE` locks the rows it reads, and an empty
/// project has none — so the statement meant to serialise allocation took no
/// lock at all in precisely the case that needed one. Two concurrent creates in
/// a new project both read 1, and the UNIQUE then turned one of them into a
/// failure.
///
/// A counter ROW always exists to be locked, which is the whole difference.
/// `next_number` is the last number HANDED OUT, so a fresh project starts at 0
/// and its first allocation is 1.
///
/// The backfill does NOT filter on deleted_at: a deleted task keeps its number,
/// and reissuing one would point an old reference at a different task.
fn task_number_counter() -> Migration {
    Migration {
        version: 3,
        name: "task_number_counter".into(),
        sql: "CREATE TABLE task_counter (
                  project_id  VARCHAR(255) NOT NULL PRIMARY KEY,
                  next_number INT UNSIGNED NOT NULL
              ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
              INSERT INTO task_counter (project_id, next_number)
                   SELECT project_id, MAX(number) FROM task GROUP BY project_id"
            .into(),
    }
}

/// D9: a repeated key is a replay, so the ORIGINAL outcome has to still exist to
/// be returned. `response` is the encoded response message and `rpc` names which
/// one it is — decoding a stored CreateTaskResponse as a DeleteTaskResponse
/// would succeed and mean nothing.
///
/// Keyed on (project, user, key) rather than on the key alone: the key is
/// CLIENT-supplied, so two clients will eventually choose the same string, and
/// deduplicating across users would hand one of them the other's record.
fn task_write_idempotency() -> Migration {
    Migration {
        version: 4,
        name: "task_write_idempotency".into(),
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
    }
}

/// `visibility` was bound from the request with `unwrap_or(1)`, and a caller
/// that set nothing sent 0 — so rows may carry VISIBILITY_UNSPECIFIED, which
/// common.proto says is never persisted. Nothing read the column, so nothing
/// noticed.
///
/// It matters NOW because something does read it. A ladder written as three
/// equality arms matches such a row on none of them, and the row becomes
/// unreadable by everyone including its owner — the fix for a leak turning into
/// a quiet loss of access. The predicate fails closed to PRIVATE as well; this
/// heals the stored rows so the invariant is true in the table rather than only
/// compensated for in every query.
///
/// Idempotent, and a no-op on any database created after the module began
/// assigning visibility itself.
fn heal_unspecified_visibility() -> Migration {
    Migration {
        version: 5,
        name: "heal_unspecified_visibility".into(),
        sql: "UPDATE task SET visibility = 1 WHERE visibility NOT IN (1, 2, 3)".into(),
    }
}

/// The column D9's amendment needs, and the one O21 books as this module's
/// debt.
///
/// A uniqueness constraint on the key detects a REPEAT; it cannot detect a
/// DIFFERING repeat. `task_write` kept the prior RESPONSE and never the prior
/// request, so comparing payloads was structurally impossible here — which is
/// why a key reused with a different payload was replayed and the operation the
/// caller actually asked for was discarded.
///
/// A SHA-256 digest, so it is exactly 32 bytes and `BINARY(32)` never pads.
///
/// **NULLABLE, and that is the load-bearing part.** Every row written before
/// this migration has no fingerprint, and NULL is the only value that means
/// "there is nothing to compare against". `NOT NULL DEFAULT ''` would make an
/// identical retry of a pre-migration key mismatch the empty string and be
/// REFUSED — D9's core rule regressed by the migration implementing D9's
/// amendment. An absent fingerprint replays, exactly as it did before.
fn task_write_request_fingerprint() -> Migration {
    Migration {
        version: 6,
        name: "task_write_request_fingerprint".into(),
        sql: "ALTER TABLE task_write
                ADD COLUMN request_fingerprint BINARY(32) NULL DEFAULT NULL"
            .into(),
    }
}
