//! `database.acquireTimeoutSeconds`'s own ceiling, pinned against the ceiling
//! a CALLER already imposes.
//!
//! Card C-DB2 (ADR-0837) moved the acquire wait out of `yadgar-store` and into
//! this chart. ADR-1006 then ruled that the timeout chain shrinks inward: each
//! hop's timeout is strictly below its caller's. `task` reaches this module
//! over `yadgar-dial`, and its request timeout is a chart value in `task`'s
//! chart (ADR-0925 Q-C1, ADR-1006 default 14s), which this chart cannot read. The
//! lowest value dial accepts for it is `min_request_timeout()`, also 14s. So
//! the bound here is `1..13` and the shipped value is 12: below every
//! deadline any caller can set. An acquire wait equal to or longer than the
//! caller's deadline could never surface as THIS pool's error: the caller's
//! timeout would always land first, one hop further from the cause.
//!
//! **THIS BINARY NEVER DIALS ANYTHING.** `yadgar-dial` is linked for
//! `Deadline` alone (ADR-1019, `src/txn.rs`), and this test reads its floor
//! back (`Cargo.toml`).
//!
//! **EVERY NUMBER IS PINNED BY A LITERAL (ADR-0599),** the same discipline
//! `tests/chart_grace_period.rs` already follows: the schema's `13` and the
//! shipped `12` are read out of the real files rather than duplicated as
//! constants here, `yadgar-dial`'s `14` is asserted against the function
//! rather than assumed, and the relation between all three is asserted
//! last.

use std::path::PathBuf;
use std::time::Duration;

/// A top-level-of-`database` scalar in `chart/values.yaml`.
///
/// Matched on a TWO-SPACE INDENT, the exact depth every key directly under
/// `database:` is written at — a plain substring search would also match
/// the key's own name inside a prose comment above it, the same trap
/// `tests/chart_grace_period.rs::chart_scalar` avoids for the root.
fn database_scalar(key: &str) -> u64 {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chart/values.yaml");
    let values = std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("{} must be readable: {why}", path.display()));

    let prefix = format!("  {key}:");
    let found: Vec<&str> = values
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(|rest| rest.split('#').next().unwrap_or_default().trim())
        .collect();

    assert_eq!(
        found.len(),
        1,
        "expected exactly one `  {key}:` directly under `database:` in {}, found {}: {found:?}",
        path.display(),
        found.len()
    );

    found[0].parse().unwrap_or_else(|why| {
        panic!(
            "`{key}: {}` in {} must be a whole number of seconds: {why}",
            found[0],
            path.display()
        )
    })
}

/// A numeric field out of ONE property object in `chart/values.schema.json`,
/// found by brace-depth matching rather than a JSON parser — this crate
/// takes no JSON dependency, and the schema's own shape (one pretty-printed
/// object per leaf) makes a parser unnecessary for reading one field back.
fn schema_object(key: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chart/values.schema.json");
    let schema = std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("{} must be readable: {why}", path.display()));

    let needle = format!("\"{key}\": {{");
    let start = schema
        .find(&needle)
        .unwrap_or_else(|| panic!("{key:?} has no object in {}", path.display()))
        + needle.len();

    let mut depth: i32 = 1;
    for (i, ch) in schema[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return schema[start..start + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("{key:?}'s object in {} never closes", path.display());
}

fn schema_u64(key: &str, field: &str) -> u64 {
    let object = schema_object(key);
    let needle = format!("\"{field}\": ");
    let rest = object
        .split(&needle)
        .nth(1)
        .unwrap_or_else(|| panic!("database.{key}'s schema object has no {field:?}: {object}"));
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or_else(|why| {
        panic!("database.{key}.{field} = {digits:?} is not a whole number: {why}")
    })
}

#[test]
fn the_acquire_timeout_bound_and_the_shipped_value_both_stay_below_the_callers_own_request_deadline(
) {
    // THE LITERAL FIRST (ADR-1006). The caller's request timeout is a chart
    // value in the CALLER's chart (`task`'s), which this chart cannot read.
    // What it can read is the lowest value any caller can set:
    // `yadgar_dial::min_request_timeout()`, one second above keepalive
    // detection, and also the caller's ADR-1006 default. An acquire wait below
    // that floor is below every caller's deadline. If dial ever moves its
    // floor, this reds and says so rather than measuring against a number
    // that moved.
    let caller_floor = yadgar_dial::min_request_timeout();
    assert_eq!(
        caller_floor,
        Duration::from_secs(14),
        "yadgar-dial's lowest request timeout moved to {}s; re-derive \
         chart/values.schema.json's database.acquireTimeoutSeconds maximum (and its shipped \
         value) against it rather than leaving a bound measured against a number that no \
         longer holds",
        caller_floor.as_secs()
    );

    let schema_max = schema_u64("acquireTimeoutSeconds", "maximum");
    let shipped = database_scalar("acquireTimeoutSeconds");

    assert_eq!(
        schema_max, 13,
        "chart/values.schema.json's database.acquireTimeoutSeconds bound moved"
    );
    assert_eq!(
        shipped, 12,
        "chart/values.yaml's shipped database.acquireTimeoutSeconds moved"
    );

    // THE RELATION THAT MATTERS (ADR-1006: each hop's timeout is strictly
    // below its caller's): an acquire wait at or above the caller's own
    // deadline could never surface as THIS pool's error, because the
    // caller's timeout always wins first.
    assert!(
        schema_max < caller_floor.as_secs(),
        "the schema allows database.acquireTimeoutSeconds up to {schema_max}s, which is not \
         below the lowest caller request deadline yadgar-dial accepts ({}s)",
        caller_floor.as_secs()
    );
    assert!(
        shipped < caller_floor.as_secs(),
        "the shipped database.acquireTimeoutSeconds ({shipped}s) is not below the lowest \
         caller request deadline yadgar-dial accepts ({}s)",
        caller_floor.as_secs()
    );
}
