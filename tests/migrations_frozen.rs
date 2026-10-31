//! Every migration task-db has shipped stays byte for byte as it shipped
//! (yaadgaar #488, #249).
//!
//! `yadgar-store` records a SHA-256 of each applied migration's exact bytes and
//! refuses to boot when a recorded digest differs from the binary's. So a
//! cosmetic edit to an applied migration (a re-indent, a move to another
//! module, a change to a `format!` constant its SQL is built from) reaches no
//! deployed store as new SQL and instead stops every one of them booting.
//!
//! The digests are of each migration's SQL as `origin/main` carried it when this
//! file was written. They are literals, never derived at run time. The
//! comparison uses `Migration::checksum`, the same function the boot check
//! calls, so this test and the boot refusal cannot disagree about what a digest
//! is.

use yadgar_task_db::schema;

const FROZEN: [(u64, &str, &str); 8] = [
    (
        1,
        "create_task",
        "c28d78fb3d22f844a041bb600d5e372b40106e2a0c81d224764a779c10b32650",
    ),
    (
        2,
        "task_tags_and_links",
        "c3d971ff80cca907cdb0163a33dbbfe141d84e5a53767938f8b6c52ffe00771d",
    ),
    (
        3,
        "task_number_counter",
        "14988ad47e3491ecf2e9ee9aac46cd123bad7e761c64839ee14c9b31df0404da",
    ),
    (
        4,
        "task_write_idempotency",
        "ecdb042e35c01bec0816e0fcc808f875f777040dcb3e066c7294ee2e62bd87a0",
    ),
    (
        5,
        "heal_unspecified_visibility",
        "21ead7b5e89100d4e5f24f12c7c302128313797db6147fc42fa662642d4c3351",
    ),
    (
        6,
        "task_write_request_fingerprint",
        "427ed3a238343f126b860158fa8f2a8f50d52a569635166745459b74e59cea62",
    ),
    (
        7,
        "task_title_at_most_128",
        "584d0a1ff3c63b544c76a386884399c1d645777a86528a0ee55683b4cda9c543",
    ),
    (
        8,
        "task_write_caller_fingerprint",
        "486db4094b285a85e6d8ef5720e647f2de5b5d671a58af2e50ed43ddafadaa3b",
    ),
];

#[test]
fn every_shipped_migration_is_unchanged() {
    let set = schema::migrations().expect("set");
    let all: Vec<_> = set.pending_after(0).collect();
    for (version, name, digest) in FROZEN {
        let m = all
            .iter()
            .find(|m| m.version == version)
            .unwrap_or_else(|| panic!("migration {version} is gone"));
        assert_eq!(m.name, name, "migration {version} was renamed");
        assert_eq!(
            m.checksum(),
            digest,
            "migration {version} ({name}) was edited"
        );
    }
}

#[test]
fn the_set_holds_exactly_the_frozen_migrations() {
    // A new migration is added here deliberately, with its digest, in the PR
    // that adds it. Forgetting to is the failure this assertion names.
    let set = schema::migrations().expect("set");
    let expected: Vec<u64> = FROZEN.iter().map(|(v, _, _)| *v).collect();
    assert_eq!(set.versions(), expected);
}
