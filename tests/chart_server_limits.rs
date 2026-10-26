//! `server.limits`: the chart's shipped defaults, pinned against the library's
//! own ranges (ADR-1006, yaadgaar #218).
//!
//! `yadgar-lifecycle` takes the listener's four protocol limits as parameters and
//! has NO default (ADR-0569, ADR-0705). The defaults live in
//! `chart/values.yaml`, the ranges live in the library as
//! `ServerLimits::*` consts, and `chart/values.schema.json` repeats the ranges
//! so a render refuses an out-of-range value before a pod does. Three copies of
//! one contract drift unless something holds them together; this file does.
//!
//! **EVERY NUMBER IS PINNED BY A LITERAL (ADR-0599),** the discipline
//! `tests/chart_request_deadline.rs` follows: the defaults are read out of the
//! real files and compared with the numbers ADR-1006 ruled, and the relation to
//! the library's range is asserted last, so a default that moved fails on its
//! own name before the range check can agree with it.

use std::ops::RangeInclusive;
use std::path::PathBuf;

use yadgar_lifecycle::serve_tls::ServerLimits;

/// One limit: its chart key, the variable the chart renders, ADR-1006's
/// default, and the library's range.
struct Limit {
    key: &'static str,
    var: &'static str,
    default: u64,
    range: RangeInclusive<u64>,
}

fn limits() -> [Limit; 4] {
    [
        Limit {
            key: "maxConcurrentStreams",
            var: "LISTEN_MAX_CONCURRENT_STREAMS",
            default: 256,
            range: ServerLimits::MAX_CONCURRENT_STREAMS,
        },
        Limit {
            key: "keepaliveIntervalSeconds",
            var: "LISTEN_KEEPALIVE_INTERVAL_SECS",
            default: 30,
            range: ServerLimits::KEEPALIVE_INTERVAL_SECS,
        },
        Limit {
            key: "keepaliveTimeoutSeconds",
            var: "LISTEN_KEEPALIVE_TIMEOUT_SECS",
            default: 10,
            range: ServerLimits::KEEPALIVE_TIMEOUT_SECS,
        },
        Limit {
            key: "maxHeaderListBytes",
            var: "LISTEN_MAX_HEADER_LIST_BYTES",
            default: 16384,
            range: ServerLimits::MAX_HEADER_LIST_BYTES,
        },
    ]
}

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("{} must be readable: {why}", path.display()))
}

/// A scalar directly under `server.limits:` in `chart/values.yaml`, matched on
/// its FOUR-SPACE indent so the key's name inside a prose comment never counts.
fn shipped(key: &str) -> u64 {
    let values = read("chart/values.yaml");
    let prefix = format!("    {key}:");
    let found: Vec<&str> = values
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(|rest| rest.split('#').next().unwrap_or_default().trim())
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `    {key}:` under `server.limits:` in chart/values.yaml, \
         found {}: {found:?}",
        found.len()
    );
    found[0]
        .parse()
        .unwrap_or_else(|why| panic!("`{key}: {}` must be a whole number: {why}", found[0]))
}

/// One property object out of `chart/values.schema.json`, by brace depth: this
/// crate takes no JSON dependency, and one pretty-printed object per leaf makes
/// a parser unnecessary.
fn schema_object(key: &str) -> String {
    let schema = read("chart/values.schema.json");
    let needle = format!("\"{key}\": {{");
    let start = schema
        .find(&needle)
        .unwrap_or_else(|| panic!("{key:?} has no object in chart/values.schema.json"))
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
    panic!("{key:?}'s object never closes");
}

fn schema_u64(key: &str, field: &str) -> u64 {
    let object = schema_object(key);
    let needle = format!("\"{field}\": ");
    let rest = object
        .split(&needle)
        .nth(1)
        .unwrap_or_else(|| panic!("{key}'s schema object has no {field:?}: {object}"));
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse()
        .unwrap_or_else(|why| panic!("{key}.{field} = {digits:?} is not a number: {why}"))
}

#[test]
fn the_shipped_defaults_are_the_numbers_adr_1006_ruled() {
    for limit in limits() {
        assert_eq!(
            shipped(limit.key),
            limit.default,
            "chart/values.yaml's server.limits.{} moved off ADR-1006's {}",
            limit.key,
            limit.default
        );
    }
}

#[test]
fn every_shipped_default_sits_inside_the_librarys_range() {
    for limit in limits() {
        let value = shipped(limit.key);
        assert!(
            limit.range.contains(&value),
            "server.limits.{} ships {value}, outside yadgar-lifecycle's {:?}: the pod would \
             refuse to boot on its own default",
            limit.key,
            limit.range
        );
    }
}

#[test]
fn the_schema_bounds_are_the_librarys_range() {
    for limit in limits() {
        assert_eq!(
            schema_u64(limit.key, "minimum"),
            *limit.range.start(),
            "chart/values.schema.json's {} minimum is not the library's",
            limit.key
        );
        assert_eq!(
            schema_u64(limit.key, "maximum"),
            *limit.range.end(),
            "chart/values.schema.json's {} maximum is not the library's",
            limit.key
        );
        assert!(
            schema_object(limit.key).contains("\"type\": \"integer\""),
            "chart/values.schema.json's {} must be an integer",
            limit.key
        );
    }
}

#[test]
fn the_shipped_keepalive_timeout_is_below_its_interval() {
    // The relation the render check refuses and the library enforces at boot,
    // asserted on the defaults so a shipped pair can never trip either.
    assert!(
        shipped("keepaliveTimeoutSeconds") < shipped("keepaliveIntervalSeconds"),
        "the shipped keepalive timeout must be strictly below the interval"
    );
}

#[test]
fn the_deployment_renders_each_variable_from_its_own_value() {
    let deployment = read("chart/templates/deployment.yaml");
    for limit in limits() {
        let wanted = format!(
            "- name: {}\n              value: {{{{ .Values.server.limits.{} | int64 | quote }}}}",
            limit.var, limit.key
        );
        assert!(
            deployment.contains(&wanted),
            "chart/templates/deployment.yaml must render {} from server.limits.{}",
            limit.var,
            limit.key
        );
    }
}
