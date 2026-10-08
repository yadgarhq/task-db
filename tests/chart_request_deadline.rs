//! `database.acquireTimeoutSeconds`'s own ceiling, pinned against the ceiling
//! a CALLER already imposes.
//!
//! Card C-DB2 (ADR-0837): `yadgar-store` v0.4.0 deletes the acquire wait's
//! compiled-in 30s default and this chart states it instead, bounded
//! `1..29`. **THE BOUND IS NOT ARBITRARY.** `task` reaches this module over
//! `yadgar-dial`, whose `default_request_timeout()` is this process's own
//! whole request deadline from the OTHER side — 30s. An acquire wait equal
//! to or longer than that deadline could never surface as THIS pool's own
//! error: the caller's timeout would always land first, one hop further
//! from the cause, the same class of defect `boot::probe_connect_options`'s
//! own doc comment names for a probe that built its own connection options.
//! 29 leaves one second of margin; the shipped `25` leaves five.
//!
//! **THIS BINARY NEVER DIALS ANYTHING.** `yadgar-dial` is a dev-dependency
//! only, named for this one comparison and nothing it ships (`Cargo.toml`).
//!
//! **EVERY NUMBER IS PINNED BY A LITERAL (ADR-0599),** the same discipline
//! `tests/chart_grace_period.rs` already follows: the schema's `29` and the
//! shipped `25` are read out of the real files rather than duplicated as
//! constants here, `yadgar-dial`'s `30` is asserted against the function
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
    // THE LITERAL FIRST. If `yadgar-dial` ever moves its own request
    // deadline, this is the assertion that reds and says so, rather than
    // the comparison below quietly measuring against a number that moved.
    let caller_deadline = yadgar_dial::default_request_timeout();
    assert_eq!(
        caller_deadline,
        Duration::from_secs(30),
        "yadgar-dial's own request deadline moved to {}s; re-derive \
         chart/values.schema.json's database.acquireTimeoutSeconds maximum (and its shipped \
         value) against it rather than leaving a bound measured against a number that no \
         longer holds",
        caller_deadline.as_secs()
    );

    let schema_max = schema_u64("acquireTimeoutSeconds", "maximum");
    let shipped = database_scalar("acquireTimeoutSeconds");

    assert_eq!(
        schema_max, 29,
        "chart/values.schema.json's database.acquireTimeoutSeconds bound moved"
    );
    assert_eq!(
        shipped, 25,
        "chart/values.yaml's shipped database.acquireTimeoutSeconds moved"
    );

    // THE RELATION THAT MATTERS: an acquire wait at or above the caller's
    // own deadline could never surface as THIS pool's error, because the
    // caller's timeout always wins first.
    assert!(
        schema_max < caller_deadline.as_secs(),
        "the schema allows database.acquireTimeoutSeconds up to {schema_max}s, which is not \
         below yadgar-dial's {}s request deadline",
        caller_deadline.as_secs()
    );
    assert!(
        shipped < caller_deadline.as_secs(),
        "the shipped database.acquireTimeoutSeconds ({shipped}s) is not below yadgar-dial's \
         {}s request deadline",
        caller_deadline.as_secs()
    );
}
