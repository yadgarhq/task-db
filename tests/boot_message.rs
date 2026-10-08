//! What an operator reads when the boot refuses.
//!
//! `main` used to return `Result<(), Box<dyn Error>>`, and Rust prints a `main`
//! that returns `Err` with DEBUG: a `BootError` came out as its variant name —
//! `ObsoleteRequireTls`, `MigrationLockWait { .. }` — and even a sentence came
//! out quoted and escaped. `main` now calls `run()` and prints `Error: {e}` with
//! Display, which is what these tests hold: the sentence names the knob and says
//! what to set, which is the whole of what ADR-0569 asks a refusal to carry. `boot`'s unit tests prove the sentences
//! exist; only running the BINARY proves they reach the operator.
//!
//! No engine is needed: every case here is refused before anything connects.

use std::process::Command;

/// Run the real binary with exactly `vars` in its environment, and return what
/// it printed to stderr. It must exit non-zero: these are all refusals.
fn refusal(vars: &[(&str, &str)]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_yadgar-task-db"))
        .env_clear()
        .envs(vars.iter().copied())
        .output()
        .expect("the test rig could not start the binary");
    assert!(
        !out.status.success(),
        "a refused boot must exit non-zero: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // PLAIN DISPLAY, NOT DEBUG OF A STRING. Rust prints `main`'s `Err` with
    // Debug, so even a refusal converted to its sentence arrived wrapped in
    // quotes with every inner quote escaped: `Error: "… is \"0\" …"`.
    let line = stderr
        .lines()
        .rfind(|l| l.starts_with("Error: "))
        .unwrap_or_else(|| panic!("no `Error: ` line on stderr: {stderr}"));
    assert!(
        !line.starts_with("Error: \"") && !line.contains("\\\""),
        "the refusal was printed as a Debug string: {line}"
    );
    stderr
}

/// `pool_config` refuses this key before reading any other, so nothing else
/// needs to be set. Its variant has no fields: Debug prints the bare name.
#[test]
fn the_obsolete_tls_key_is_refused_with_its_sentence_not_its_variant_name() {
    let stderr = refusal(&[("DB_REQUIRE_TLS", "true")]);
    assert!(
        stderr.contains("Set DB_SSL_MODE"),
        "the refusal must tell the operator what to set: {stderr}"
    );
    assert!(
        !stderr.contains("ObsoleteRequireTls"),
        "the operator got the Debug variant name, not the sentence: {stderr}"
    );
}

/// `DB_PORT` is the first PARSED knob `pool_config` reads, so a malformed
/// value there needs only `DB_HOST` set beside it to reach its own refusal
/// (ledgers 1257, 748; card C-DB1). Today's `Int(#[from] ParseIntError)`
/// would have printed the bare parse error with neither the key nor the
/// chart key in it.
#[test]
fn a_malformed_db_port_names_the_variable_and_the_chart_key() {
    let stderr = refusal(&[("DB_HOST", "engine.example.invalid"), ("DB_PORT", "abc")]);
    assert!(
        stderr.contains("DB_PORT"),
        "the refusal must name the variable: {stderr}"
    );
    assert!(
        stderr.contains("database.port"),
        "the refusal must name the chart key: {stderr}"
    );
    assert!(
        !stderr.contains("Unparsable"),
        "the operator got the Debug variant, not the sentence: {stderr}"
    );
}

/// `DB_ENGINE_OPERATOR_RESERVE` is one of the four knobs card C-DB2 adds
/// (ADR-0837, ADR-0849): `yadgar-store` v0.4.0 deleted the `5` it used to
/// compile in, so an absent value here must refuse the boot naming both the
/// variable and the chart key, the same shape every other required knob
/// already takes — not silently reach for the old constant, which no longer
/// exists to reach for.
#[test]
fn an_unset_db_engine_operator_reserve_names_the_variable_and_the_chart_key() {
    let stderr = refusal(&[
        ("DB_HOST", "engine.example.invalid"),
        ("DB_PORT", "13306"),
        ("DB_NAME", "task_fixture"),
        ("DB_USER", "task_fixture_user"),
        ("DB_MAX_CONNECTIONS", "4"),
        ("REPLICAS", "3"),
        ("DB_ENGINE_MAX_CONNECTIONS", "200"),
    ]);
    assert!(
        stderr.contains("DB_ENGINE_OPERATOR_RESERVE"),
        "the refusal must name the variable: {stderr}"
    );
    assert!(
        stderr.contains("database.engineOperatorReserve"),
        "the refusal must name the chart key: {stderr}"
    );
}

/// `DB_ACQUIRE_TIMEOUT_SECONDS` is read before `DB_SSL_MODE` now, so a value
/// that is there but unparsable reaches its own refusal with only the seven
/// knobs ahead of it set (card C-DB2).
#[test]
fn a_malformed_db_acquire_timeout_seconds_names_the_variable_and_the_chart_key() {
    let stderr = refusal(&[
        ("DB_HOST", "engine.example.invalid"),
        ("DB_PORT", "13306"),
        ("DB_NAME", "task_fixture"),
        ("DB_USER", "task_fixture_user"),
        ("DB_MAX_CONNECTIONS", "4"),
        ("REPLICAS", "3"),
        ("DB_ENGINE_MAX_CONNECTIONS", "200"),
        ("DB_ENGINE_OPERATOR_RESERVE", "5"),
        ("DB_ACQUIRE_TIMEOUT_SECONDS", "abc"),
    ]);
    assert!(
        stderr.contains("DB_ACQUIRE_TIMEOUT_SECONDS"),
        "the refusal must name the variable: {stderr}"
    );
    assert!(
        stderr.contains("database.acquireTimeoutSeconds"),
        "the refusal must name the chart key: {stderr}"
    );
    assert!(
        !stderr.contains("Unparsable"),
        "the operator got the Debug variant, not the sentence: {stderr}"
    );
}

/// `LISTEN_TLS_ENABLED` is read right after the migration lock's wait, so a
/// full pool environment plus a usable wait reaches its own refusal and
/// nothing later (ADR-0845; card C-DB1). Before this card, an absent value
/// here produced a cleartext listener with no refusal at all.
///
/// The four C-DB2 knobs are part of that "full pool environment" now
/// (`pool_config` reads them before `DB_SSL_MODE`), so this fixture carries
/// them too — otherwise the refusal this test wants would never be reached.
#[test]
fn an_unset_listen_tls_enabled_names_the_variable_and_the_chart_key() {
    let stderr = refusal(&[
        ("DB_HOST", "engine.example.invalid"),
        ("DB_PORT", "13306"),
        ("DB_NAME", "task_fixture"),
        ("DB_USER", "task_fixture_user"),
        ("DB_MAX_CONNECTIONS", "4"),
        ("REPLICAS", "3"),
        ("DB_ENGINE_MAX_CONNECTIONS", "200"),
        ("DB_ENGINE_OPERATOR_RESERVE", "5"),
        ("DB_ACQUIRE_TIMEOUT_SECONDS", "25"),
        ("DB_IDLE_TIMEOUT_SECONDS", "600"),
        ("DB_MAX_LIFETIME_SECONDS", "1800"),
        ("DB_SSL_MODE", "verify-identity"),
        ("DB_MIGRATION_LOCK_TIMEOUT_SECONDS", "60"),
    ]);
    assert!(
        stderr.contains("LISTEN_TLS_ENABLED"),
        "the refusal must name the variable: {stderr}"
    );
    assert!(
        stderr.contains("tls.enabled"),
        "the refusal must name the chart key: {stderr}"
    );
}

/// The migration lock's wait is read right after the pool's knobs, so a full
/// pool environment plus an unusable wait reaches that refusal and nothing
/// later. Its variant carries fields: Debug would print `MigrationLockWait {
/// value: "0", .. }` and name neither the variable nor the chart key.
///
/// The four C-DB2 knobs are part of the pool's own knobs now, so this
/// fixture states them too (card C-DB2).
#[test]
fn an_unusable_migration_lock_wait_is_refused_naming_the_variable_and_the_chart_key() {
    let stderr = refusal(&[
        ("DB_HOST", "engine.example.invalid"),
        ("DB_PORT", "13306"),
        ("DB_NAME", "task_fixture"),
        ("DB_USER", "task_fixture_user"),
        ("DB_MAX_CONNECTIONS", "4"),
        ("REPLICAS", "3"),
        ("DB_ENGINE_MAX_CONNECTIONS", "200"),
        ("DB_ENGINE_OPERATOR_RESERVE", "5"),
        ("DB_ACQUIRE_TIMEOUT_SECONDS", "25"),
        ("DB_IDLE_TIMEOUT_SECONDS", "600"),
        ("DB_MAX_LIFETIME_SECONDS", "1800"),
        ("DB_SSL_MODE", "verify-identity"),
        ("DB_MIGRATION_LOCK_TIMEOUT_SECONDS", "0"),
    ]);
    assert!(
        stderr.contains("DB_MIGRATION_LOCK_TIMEOUT_SECONDS is"),
        "the refusal must name the variable: {stderr}"
    );
    assert!(
        stderr.contains("database.migrationLockTimeoutSeconds"),
        "the refusal must name the chart key: {stderr}"
    );
    assert!(
        !stderr.contains("MigrationLockWait"),
        "the operator got the Debug variant, not the sentence: {stderr}"
    );
}
