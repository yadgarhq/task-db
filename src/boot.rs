//! What `main` decides before it opens anything — in a place a test can reach.
//!
//! `main` is a binary entry point, so nothing in it is reachable from a test.
//! That is fine for wiring and not fine for decisions, and three decisions here
//! are exactly the kind that fail silently: which transport mode the connections
//! use, what happens to an environment key that no longer means anything, and
//! which transport this module SERVES on. All three live in this module, and all
//! three have a test.
//!
//! **A FOURTH USED TO, AND IT LEFT — WHICH SIGNALS END THIS PROCESS.** `main`
//! passed `tokio::signal::ctrl_c()` to `serve_with_shutdown` and there was no
//! `signal::unix` handler anywhere in this crate, so the drain ran on SIGINT,
//! which Kubernetes never sends, and not on SIGTERM, which is the only signal it
//! does send. It was wrong from the day it was written, it was one line, and
//! nothing in the repository could see it because the line lived where no test
//! reaches. That is the argument this module is built on, and it argues for a
//! SHARED answer once the same line exists in five repositories: the estate had
//! five copies of `shutdown` and had to correct four of them separately.
//! `yadgar-lifecycle` v0.1.0 holds it now (D19, ADR-0526).
//!
//! What remains here is [`shutdown`], three lines wrapping that crate so the
//! refusal reaches an operator as a [`BootError`] sentence like every other
//! refusal in this module — the ONE thing about it that was ever specific to
//! `task-db`. `tests/shutdown.rs` still proves the drain against this service's
//! own listener.
//!
//! **The connection options are the point.** D7's capability probe runs on a
//! connection of its own, before the pool exists. This binary used to build that
//! connection by `format!`-ing `mysql://user:pass@host:port/db`, with no
//! `ssl-mode` in it — so it inherited sqlx's default, `Preferred`, which sqlx
//! documents as falling back to an unencrypted connection when an encrypted one
//! cannot be established, while the pool beside it was on `Required`. Two code
//! paths that must agree about TLS was the bug; one path is the fix, and
//! [`probe_connect_options`] is the seam that keeps it one.
//!
//! **The listener is the same argument, one hop further out.** `DB_SSL_MODE`
//! decides how this module reaches its engine; [`ServerTls`] decides what `task`
//! gets when it reaches this module. NEITHER DEFAULTS ANY MORE: `DB_SSL_MODE` is
//! required and refuses the boot when unset (ADR-0569), and the listener's
//! switch and client-auth mode are both required (ADR-0845, ADR-0854). Both refuse rather than
//! downgrade when asked for something they cannot deliver, and neither names an
//! issuer, a CRD or a mesh (D80) — a flag and file paths is the whole of the
//! configuration.

use std::path::PathBuf;
use std::time::Duration;

use sqlx::mysql::MySqlConnectOptions;
use yadgar_store::credentials::Secret;
// NO `DEFAULT_SSL_MODE`. It was the fallback `pool_config` handed
// `parse_ssl_mode` when DB_SSL_MODE was unset, and under ADR-0569 there is no
// such position to hand anything to. The constant still exists in
// `yadgar-store` and this module is simply no longer one of its readers.
use yadgar_store::pool::{parse_ssl_mode, PoolConfig, PoolError};

// THE LISTENER'S TRANSPORT, in a file of its own. `boot.rs` crossed the shared
// 500-line ceiling, and this is the seam `tests/serve_tls.rs` already names: what
// this module SERVES on, as against the connection OUT to the engine configured
// below. Since B-U5 the file holds only the WIRING of
// `yadgar_lifecycle::serve_tls`; the types are lifecycle's, re-exported so
// `boot::listener`, `boot::server` and `boot::refusal` are the paths `main` and
// every test write.
mod serve_tls;

pub use serve_tls::{
    listener, refusal, server, ClientAuth, ServeTlsError, ServerTls, LISTEN, LISTEN_CHART_KEY,
};

/// The key this module used to read, and no longer does.
///
/// Named as a constant because it appears in the refusal below and nowhere else
/// — the only remaining reason this string exists is to be refused.
const OBSOLETE_TLS_KEY: &str = "DB_REQUIRE_TLS";

/// The key that replaced it.
const SSL_MODE_KEY: &str = "DB_SSL_MODE";

/// The key naming the authority the verifying modes check the engine against.
///
/// **`DB_SSL_*` rather than `DB_TLS_*`, and the difference is deliberate.** The
/// prefix rule this module already follows for [`LISTEN`] gives `DB` either way
/// — a connection OUT is named for what it reaches. What differs is the middle
/// word, and the gate decides it. Every `<UPSTREAM>_TLS_*` family in the estate,
/// including the `TASK_DB_TLS_*` that `task` uses to reach THIS module, is gated
/// by a boolean `_TLS_ENABLED`. This dial has no such flag: it is gated by
/// five-valued [`SSL_MODE_KEY`], and this file is meaningful under two of those
/// values and inert under three. A `DB_TLS_CA_FILE` read beside a `DB_SSL_MODE`
/// in this same function would be two words for one concept inside one pair of
/// keys — the defect the naming rule exists to prevent rather than an instance
/// of it. `SSL` also names what it fills: sqlx's `ssl_ca`, on
/// [`yadgar_store::pool::PoolConfig::ssl_ca`].
const SSL_CA_KEY: &str = "DB_SSL_CA_FILE";

/// A knob read from ONE source, refusing rather than inventing (ADR-0569).
///
/// It replaces `env_or(env, key, default)`, and deleting the `default` parameter
/// is more of the point than the rename: while the helper took one, every knob
/// in [`pool_config`] had somewhere for a fallback to live, and a fallback is
/// invisible at the point of use, survives an upgrade unnoticed, and makes the
/// effective setting depend on which layer a reader happens to inspect.
///
/// AN EMPTY VALUE REFUSES TOO, AND WITH ITS OWN MESSAGE. A set-but-empty
/// variable and an absent one collapsing into a single branch is a defect this
/// estate found three separate times in one week. Helm renders an unset value as
/// `""`, so the empty case is what a nulled chart value actually produces, and it
/// is the one an operator is most likely to hit. It also has to be caught HERE
/// rather than by the parse: `"".parse::<u16>()` is a `ParseIntError` that names
/// no key at all, so an operator reading a crash loop would learn that some
/// number was unreadable and never which one.
///
/// THE LOOKUP IS PASSED IN, for the reason [`pool_config`] gives — a test states
/// a whole environment without mutating the process.
fn env_required(env: &impl Fn(&str) -> Option<String>, key: &str) -> Result<String, String> {
    match env(key) {
        Some(value) if !value.is_empty() => Ok(value),
        Some(_) => Err(format!(
            "{key} is set but EMPTY. It has no compiled-in default (ADR-0569), so there is \
             nothing to fall back to. The chart renders it; a values override that nulls it \
             produces exactly this."
        )),
        None => Err(format!(
            "{key} is NOT SET. It has no compiled-in default (ADR-0569): this process reads \
             it from the environment alone and refuses to start rather than invent a value. \
             The chart renders it."
        )),
    }
}

/// The chart key behind each of [`pool_config`]'s required knobs, in the
/// order `pool_config` reads them.
///
/// Named here rather than inlined at each call site, so a reader can find
/// the whole mapping in one place rather than assembling it from eight
/// scattered string literals. `REPLICAS` has no single key: the chart picks
/// `autoscaling.maxReplicas` or `replicaCount` depending on
/// `autoscaling.enabled` (see `chart/templates/deployment.yaml`), and the
/// sentence says so rather than naming only one of the two and being wrong
/// half the time.
const HOST_CHART_KEY: &str = "database.host";
const PORT_CHART_KEY: &str = "database.port";
const NAME_CHART_KEY: &str = "database.name";
const USER_CHART_KEY: &str = "database.user";
const MAX_CONNECTIONS_CHART_KEY: &str = "database.maxConnections";
const REPLICAS_CHART_KEY: &str =
    "autoscaling.maxReplicas (when autoscaling.enabled is true) or replicaCount";
const ENGINE_MAX_CONNECTIONS_CHART_KEY: &str = "database.engineMaxConnections";
const SSL_MODE_CHART_KEY: &str = "database.sslMode";

/// `yadgar-store` v0.4.0 deleted the four `PoolConfig` fields below from its
/// own compiled-in defaults (ADR-0837, ADR-0849, card C-DB2): sqlx's own
/// 30s/600s/1800s acquire/idle/lifetime, and the `5` this crate's headroom
/// check used to hard-code as `operator_reserve`. Read and named here the
/// same way the eight knobs above already are.
const OPERATOR_RESERVE_KEY: &str = "DB_ENGINE_OPERATOR_RESERVE";
const OPERATOR_RESERVE_CHART_KEY: &str = "database.engineOperatorReserve";

/// Bounded above by `chart/values.schema.json`'s `maximum: 29` — below the
/// dial client's own `REQUEST_TIMEOUT` (30s), so this pool's own acquire
/// wait can never be the deadline a caller's whole request runs against.
/// `tests/chart_request_deadline.rs` pins the two together.
const ACQUIRE_TIMEOUT_KEY: &str = "DB_ACQUIRE_TIMEOUT_SECONDS";
const ACQUIRE_TIMEOUT_CHART_KEY: &str = "database.acquireTimeoutSeconds";
const IDLE_TIMEOUT_KEY: &str = "DB_IDLE_TIMEOUT_SECONDS";
const IDLE_TIMEOUT_CHART_KEY: &str = "database.idleTimeoutSeconds";
const MAX_LIFETIME_KEY: &str = "DB_MAX_LIFETIME_SECONDS";
const MAX_LIFETIME_CHART_KEY: &str = "database.maxLifetimeSeconds";

/// [`env_required`], with the chart key appended to either of its two
/// sentences (ADR-0569). `env_required` itself says only "The chart renders
/// it" — true of every knob here, and so no help telling one apart from
/// another in a crash loop. `lock::migration_lock` already appends its own
/// chart key by hand at its one call site; `pool_config` has eight, so the
/// append is a function instead of eight repeated `format!`s.
fn env_required_named(
    env: &impl Fn(&str) -> Option<String>,
    key: &str,
    chart_key: &str,
) -> Result<String, BootError> {
    env_required(env, key).map_err(|sentence| {
        BootError::MissingKnob(format!("{sentence} Set the chart value {chart_key}."))
    })
}

/// The same knob, parsed as a whole number, naming both the variable and the
/// chart key in the PARSE refusal too — which bare `.parse()?` never did.
/// [`BootError::Unparsable`] is the whole of what this adds over
/// [`env_required_named`].
fn env_parsed<T>(
    env: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    chart_key: &'static str,
) -> Result<T, BootError>
where
    T: std::str::FromStr<Err = std::num::ParseIntError>,
{
    let raw = env_required_named(env, key, chart_key)?;
    raw.parse().map_err(|source| BootError::Unparsable {
        key,
        chart_key,
        value: raw.clone(),
        source,
    })
}

/// Read the pool configuration, refusing rather than guessing.
///
/// Takes the environment as a lookup rather than reading it directly, so a test
/// can state a whole environment without mutating the process — `std::env` is
/// global and `cargo test` runs threads in parallel.
pub fn pool_config(env: impl Fn(&str) -> Option<String>) -> Result<PoolConfig, BootError> {
    // FIRST, before anything else can fail. An operator who set DB_REQUIRE_TLS
    // to tighten transport security and got a numeric parse error about some
    // other key would fix the other key and never learn that this one is inert.
    if env(OBSOLETE_TLS_KEY).is_some() {
        return Err(BootError::ObsoleteRequireTls);
    }

    // EVERY ONE OF THE TWELVE IS REQUIRED, and every one is rendered by this
    // repository's chart — which is the half that makes the requirement safe
    // rather than a pod that will not boot. `env_required_named`/`env_parsed`
    // at each site rather than a signature change: this function's error type
    // is `BootError` and its callers read it, so the refusal joins the enum
    // as one more sentence instead of becoming a second error type beside it.
    Ok(PoolConfig {
        host: env_required_named(&env, "DB_HOST", HOST_CHART_KEY)?,
        port: env_parsed(&env, "DB_PORT", PORT_CHART_KEY)?,
        database: env_required_named(&env, "DB_NAME", NAME_CHART_KEY)?,
        username: env_required_named(&env, "DB_USER", USER_CHART_KEY)?,
        max_connections: env_parsed(&env, "DB_MAX_CONNECTIONS", MAX_CONNECTIONS_CHART_KEY)?,
        replicas: env_parsed(&env, "REPLICAS", REPLICAS_CHART_KEY)?,
        engine_max_connections: env_parsed(
            &env,
            "DB_ENGINE_MAX_CONNECTIONS",
            ENGINE_MAX_CONNECTIONS_CHART_KEY,
        )?,
        // THE OTHER HALF OF D4's ARITHMETIC (ADR-0849). It used to be `5`,
        // compiled into `store`, and the right number depends on the engine
        // an adopter runs and on what else connects to it — a deployment
        // fact, same as `engine_max_connections` beside it.
        operator_reserve: env_parsed(&env, OPERATOR_RESERVE_KEY, OPERATOR_RESERVE_CHART_KEY)?,
        // THE THREE POOL DURATIONS `store` used to inherit from sqlx's own
        // defaults (30s/600s/1800s) rather than state. Read as whole seconds,
        // like every other duration this module renders, and converted once
        // here rather than asking `store` to parse a unit it does not own.
        acquire_timeout: Duration::from_secs(env_parsed(
            &env,
            ACQUIRE_TIMEOUT_KEY,
            ACQUIRE_TIMEOUT_CHART_KEY,
        )?),
        idle_timeout: Duration::from_secs(env_parsed(
            &env,
            IDLE_TIMEOUT_KEY,
            IDLE_TIMEOUT_CHART_KEY,
        )?),
        max_lifetime: Duration::from_secs(env_parsed(
            &env,
            MAX_LIFETIME_KEY,
            MAX_LIFETIME_CHART_KEY,
        )?),
        ssl_mode: parse_ssl_mode(&env_required_named(&env, SSL_MODE_KEY, SSL_MODE_CHART_KEY)?)?,
        // STILL AN OPTIONAL READ, and deliberately NOT converted to
        // `env_required` with the rest (ADR-0569). The chart renders
        // DB_SSL_CA_FILE only under `database.sslCaSecret`, so requiring it would
        // refuse the boot of every deployment that never asked for certificate
        // verification — the rule's own failure mode, pointed the other way. An
        // absent authority is a correct system, which is exactly the shape
        // ADR-0569's revisit trigger names.
        //
        // TRIMMED AND EMPTY-FILTERED, unlike every value above, because this one
        // is an `Option` and Helm renders an unset value as `""`. Without the
        // filter that empty string becomes `Some(PathBuf::new())` — a path sqlx
        // opens and cannot, so a deployment that never asked for certificate
        // verification fails to boot. Absent and empty must mean the same thing:
        // no authority named, which is what `None` is. Same shape as
        // the listener's own optional paths in `yadgar_lifecycle::serve_tls`.
        ssl_ca: env(SSL_CA_KEY)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    })
}

/// The options D7's capability probe connects with.
///
/// It is `store`'s own [`yadgar_store::pool::connect_options`] and deliberately
/// nothing else — the same call the pool makes, given the same config. This
/// function adds no behaviour; it exists so that the probe's description of a
/// connection and the pool's cannot drift apart again, and so that a test can
/// say which one the probe got.
///
/// **IT RETURNS A `Result` BECAUSE `store` REFUSES A MODE HERE, and forwarding
/// that refusal unchanged is the whole of what this adds.** `verify_ca` names a
/// check sqlx does not perform — the trust store is seeded with the public web
/// roots before the configured authority is appended, and every mode but
/// `verify_identity` skips the hostname check — so it accepts any
/// publicly-trusted certificate for any name. `store` places the refusal where
/// the probe and the pool meet, which is why the probe cannot route around it by
/// building its own options. No variant is added for it: [`BootError::Pool`] is
/// `#[error(transparent)]` over `PoolError` and already carries the sentence an
/// operator reads.
pub fn probe_connect_options(
    config: &PoolConfig,
    secret: &Secret,
) -> Result<MySqlConnectOptions, BootError> {
    Ok(yadgar_store::pool::connect_options(config, secret)?)
}

/// The future `serve_with_shutdown` drains on, adapted to this module's error.
///
/// **THE BEHAVIOUR IS `yadgar_lifecycle::shutdown`'S AND NOTHING HERE CHANGES
/// IT.** SIGTERM and SIGINT, both handlers installed before the call returns,
/// the winning signal named in the log line. What survives in this repository is
/// three lines of error adaptation, and the paragraph an operator reads.
///
/// **IT COSTS MORE HERE THAN IN THE LOGIC TIER, which is why this module keeps a
/// wrapper rather than calling the crate from `main` the way `iam-db` does.** A
/// killed `task` loses requests; a killed `task-db` loses them mid-write.
/// Nothing is corrupted — an unfinished transaction rolls back, which is what
/// D8's compare-and-set already depends on — but the caller is told nothing and
/// has to infer the outcome from a severed stream. D23 sets the blast radius:
/// `task` reaches this module over ONE long-lived HTTP/2 connection per pod, so
/// what is severed is everything that connection was carrying rather than a thin
/// slice of it. Every other refusal in this module reaches the operator as a
/// SENTENCE, because `main` returns `Box<dyn Error>` and Rust prints that with
/// `Debug`; a bare `io::Error` here would land in the crash loop as
/// `Os { code: 24, .. }` and say none of the above.
///
/// **A `map_err` RATHER THAN A `From` IMPL, deliberately.** [`BootError`] would
/// need `From<std::io::Error>` for a bare `?` to work, and
/// an unreadable mounted file is an `io::Error` too, for an entirely different
/// reason — so the blanket impl would let any unreadable file become a
/// signal-handler refusal at whatever call site next wrote `?`. One explicit
/// conversion at the one place it is correct is the smaller claim.
///
/// # Errors
///
/// Registration can fail, and `main` refuses to start on it. A server that
/// cannot hear SIGTERM cannot drain, and starting anyway hides that until the
/// next rollout.
pub fn shutdown() -> Result<impl std::future::Future<Output = ()>, BootError> {
    yadgar_lifecycle::shutdown().map_err(|source| BootError::SignalHandler { source })
}

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error(
        "DB_REQUIRE_TLS is set and this binary no longer reads it. Set DB_SSL_MODE \
         instead — one of: disabled, preferred, required, verify_ca, verify_identity. \
         Refusing at boot rather than ignoring the key, because \
         an operator who set it is asking for a transport guarantee, and silently \
         substituting a default is the one outcome worse than stopping. \
         DB_REQUIRE_TLS was a boolean and could not ask for certificate \
         verification at all; verify_ca and verify_identity are why it is gone. \
         Both check the engine's certificate against the authority named by \
         DB_SSL_CA_FILE; with none named they check the PUBLIC WEB ROOTS instead, \
         which sign no operator-issued engine certificate. Set both keys together \
         or neither."
    )]
    ObsoleteRequireTls,

    // THE LISTENER'S REFUSALS ARE NOT HERE ANY MORE (card B-U5). They are
    // `yadgar_lifecycle::serve_tls::ServeTlsError`'s, each naming the
    // variable AND the chart key; `boot::refusal` is the sentence `main`
    // prints for one.

    // NO `name` FIELD, and its loss is the one thing this module gave up to stop
    // spelling `shutdown` for itself. `yadgar_lifecycle::shutdown` installs both
    // handlers and returns one `io::Error`, so WHICH of the two failed is not
    // knowable here. Naming both is the honest reading and costs the operator
    // nothing: the sentence below already says there is no value to correct and
    // that the pod restarting is the right response, so no action was ever keyed
    // to which handler it was. A fabricated "SIGTERM/SIGINT" in a field that
    // names one thing would be worse than no field.
    //
    // `iam-db`'s `main` has phrased this failure collectively since its own
    // SIGTERM work — "the SIGTERM and SIGINT handlers could not be installed" —
    // so this is the estate's existing wording rather than a new one.
    #[error(
        "the SIGTERM and SIGINT handlers could not be installed: {source}. This module \
         refuses to start rather than run without them: Kubernetes ends every pod with \
         SIGTERM, and a process that cannot hear it is one that never drains — its \
         in-flight writes are severed by the SIGKILL that follows, on every rolling \
         update, with nothing in the logs to say so. This is a broken process \
         environment rather than a configuration mistake, so there is no value to \
         correct; the pod restarting is the right response."
    )]
    SignalHandler {
        #[source]
        source: std::io::Error,
    },

    // ONE VARIANT FOR ALL EIGHT KNOBS, carrying the sentence `env_required`
    // wrote. It is `#[error("{0}")]` rather than a wrapper phrase because the
    // string already names the key, says there is no compiled-in default, and
    // says the chart renders it — a prefix here would only push the key further
    // from the start of the line an operator reads in a crash loop.
    //
    // A `String` payload rather than a `&'static str` key plus a rendered
    // reason: the empty and absent cases MUST read differently (Helm renders an
    // unset value as `""`), and a variant carrying only the key could not tell
    // them apart without becoming two variants that say the same thing twice.
    #[error("{0}")]
    MissingKnob(String),

    /// The migration lock's wait is set and unusable (ledger 814, ADR-0837).
    /// Absent and empty are [`BootError::MissingKnob`]; this is a value that is
    /// there and cannot be a wait. It names the variable AND the chart key,
    /// because ADR-0569 asks a refusal to say where the knob is set. The
    /// reason goes LAST, since `store`'s ends in a full stop and a parse error's
    /// does not, and the bound is NOT restated: chart/values.schema.json is its
    /// one source.
    #[error(
        "DB_MIGRATION_LOCK_TIMEOUT_SECONDS is {value:?}, which is not a usable migration \
         lock wait. Set the chart value database.migrationLockTimeoutSeconds to a whole \
         number of seconds, at least 1, within the bound chart/values.schema.json sets. \
         Why: {reason}"
    )]
    MigrationLockWait { value: String, reason: String },

    #[error(transparent)]
    Pool(#[from] PoolError),

    /// A required knob is set, non-empty, and not a whole number
    /// (ADR-0569). [`BootError::MissingKnob`] already covers absent and
    /// empty; this is the shape neither of those two is — a value that is
    /// THERE and cannot be parsed, which `Int(#[from] ParseIntError)` used
    /// to carry with no key and no chart key, so the operator learned only
    /// that SOME number was unreadable and never which one.
    #[error(
        "{key} is {value:?}, which is not a whole number. Set the chart value \
         {chart_key} to a plain integer. Why: {source}"
    )]
    Unparsable {
        key: &'static str,
        chart_key: &'static str,
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },
}

mod lock;
pub use lock::{migration_lock, MIGRATION_LOCK_TIMEOUT_CHART_KEY, MIGRATION_LOCK_TIMEOUT_KEY};

#[cfg(test)]
mod tests;
