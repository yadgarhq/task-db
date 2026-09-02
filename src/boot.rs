//! What `main` decides before it opens anything — in a place a test can reach.
//!
//! `main` is a binary entry point, so nothing in it is reachable from a test.
//! That is fine for wiring and not fine for decisions, and three decisions here
//! are exactly the kind that fail silently: which transport mode the connections
//! use, what happens to an environment key that no longer means anything, and
//! which transport this module SERVES on. All three live in this module, and all
//! three have a test.
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
//! decides how this module reaches its engine; [`ServeTls`] decides what `task`
//! gets when it reaches this module. Both default to the transport the reference
//! deployment already runs, both refuse rather than downgrade when asked for
//! something they cannot deliver, and neither names an issuer, a CRD or a mesh
//! (D80) — a flag and file paths is the whole of the configuration.

use std::path::{Path, PathBuf};

use sqlx::mysql::MySqlConnectOptions;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use yadgar_store::credentials::Secret;
use yadgar_store::pool::{parse_ssl_mode, PoolConfig, PoolError, DEFAULT_SSL_MODE};

/// The key this module used to read, and no longer does.
///
/// Named as a constant because it appears in the refusal below and nowhere else
/// — the only remaining reason this string exists is to be refused.
const OBSOLETE_TLS_KEY: &str = "DB_REQUIRE_TLS";

/// The key that replaced it.
const SSL_MODE_KEY: &str = "DB_SSL_MODE";

/// The environment variables this module's own listener is configured from.
///
/// Built from a PREFIX rather than written out three times, so the naming stays
/// mechanical: `<PREFIX>_TLS_ENABLED`, `<PREFIX>_TLS_CERT_FILE` and
/// `<PREFIX>_TLS_KEY_FILE`. `SERVE` rather than the bare names, because `DB_*`
/// already means "the engine this module talks to" in this process — a bare
/// `TLS_ENABLED` would be ambiguous between the two directions, and `task` reads
/// the same `SERVE_` prefix for the same reason.
pub const SERVE: &str = "SERVE";

fn env_or(env: &impl Fn(&str) -> Option<String>, key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
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

    Ok(PoolConfig {
        host: env_or(&env, "DB_HOST", "127.0.0.1"),
        port: env_or(&env, "DB_PORT", "3306").parse()?,
        database: env_or(&env, "DB_NAME", "task"),
        username: env_or(&env, "DB_USER", "task"),
        max_connections: env_or(&env, "DB_MAX_CONNECTIONS", "8").parse()?,
        replicas: env_or(&env, "REPLICAS", "2").parse()?,
        engine_max_connections: env_or(&env, "DB_ENGINE_MAX_CONNECTIONS", "151").parse()?,
        ssl_mode: parse_ssl_mode(&env_or(&env, SSL_MODE_KEY, DEFAULT_SSL_MODE))?,
    })
}

/// The options D7's capability probe connects with.
///
/// It is `store`'s own [`yadgar_store::pool::connect_options`] and deliberately
/// nothing else — the same call the pool makes, given the same config. This
/// function adds no behaviour; it exists so that the probe's description of a
/// connection and the pool's cannot drift apart again, and so that a test can
/// say which one the probe got.
pub fn probe_connect_options(config: &PoolConfig, secret: &Secret) -> MySqlConnectOptions {
    yadgar_store::pool::connect_options(config, secret)
}

/// The identity this module presents to callers: a certificate and its private
/// key, both as paths on disk.
///
/// **File paths, never an issuer-specific resource** (D80). cert-manager writes
/// these files in the reference deployment and a hand-assembled Secret writes
/// them anywhere else, and nothing here can tell the difference — which is the
/// point.
///
/// **No verification domain.** A client checks the name it dialled against the
/// certificate it was shown; a server presents what it was given and checks
/// nothing. `task`'s `UpstreamTls` carries a domain override for that reason and
/// this does not, which is an asymmetry rather than an omission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeTls {
    cert_file: PathBuf,
    key_file: PathBuf,
}

impl ServeTls {
    /// Read the listener's transport configuration from the environment.
    ///
    /// `Ok(None)` is the ordinary answer today: TLS is opt-in, so an
    /// unconfigured deployment serves in cleartext exactly as before.
    pub fn from_env(prefix: &'static str) -> Result<Option<Self>, BootError> {
        Self::from_lookup(prefix, |key| std::env::var(key).ok())
    }

    /// The same decision, over an injected lookup — the shape every other
    /// decision in this module already takes, and for the same reason:
    /// `std::env` is process-global, so a test that sets one variable steers
    /// every other test in the binary.
    pub fn from_lookup(
        prefix: &'static str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, BootError> {
        let get = |suffix: &str| {
            lookup(&format!("{prefix}_{suffix}"))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        // Exactly "1". A permissive parse here — "0", "false" and "no" all
        // enabling it — is how a setting meant to be off ends up on, and the
        // reverse mistake is worse: this flag is the revert lever for the
        // cut-over, and a lever that does not move is not one.
        if get("TLS_ENABLED").as_deref() != Some("1") {
            if get("TLS_CERT_FILE").is_some() || get("TLS_KEY_FILE").is_some() {
                // NOT an error. Leaving the certificate in place while the flag
                // is off is exactly how the cut-over gets reverted, so refusing
                // it would make the lever unusable. It is still worth a line: a
                // deployment that believes it is encrypted and is not should be
                // able to see that from the boot log.
                tracing::warn!(
                    prefix,
                    "a serving certificate is configured but {prefix}_TLS_ENABLED is not \
                     \"1\", so this module listens in CLEARTEXT"
                );
            }
            return Ok(None);
        }

        Ok(Some(Self {
            cert_file: PathBuf::from(get("TLS_CERT_FILE").ok_or(BootError::NoTlsCertFile(prefix))?),
            key_file: PathBuf::from(get("TLS_KEY_FILE").ok_or(BootError::NoTlsKeyFile(prefix))?),
        }))
    }

    /// The PEM certificate this module presents.
    pub fn cert_file(&self) -> &Path {
        &self.cert_file
    }

    /// The PEM private key belonging to that certificate.
    pub fn key_file(&self) -> &Path {
        &self.key_file
    }

    /// Read both files and hand tonic the pair.
    ///
    /// Reading them HERE rather than letting tonic do it is what lets the error
    /// name WHICH file was wrong. `Identity::from_pem` takes bytes and has no
    /// idea where they came from, so an operator whose Secret mounted only one
    /// of the two would otherwise be told that "an identity" was unusable.
    fn identity(&self) -> Result<Identity, BootError> {
        let cert = read_pem(&self.cert_file, "certificate")?;
        let key = read_pem(&self.key_file, "private key")?;
        Ok(Identity::from_pem(cert, key))
    }
}

fn read_pem(path: &Path, what: &'static str) -> Result<Vec<u8>, BootError> {
    std::fs::read(path).map_err(|source| BootError::TlsUnreadable {
        what,
        path: path.to_path_buf(),
        source,
    })
}

/// Build the gRPC server this module listens with.
///
/// **THE ONLY SERVER CONSTRUCTION IN THIS BINARY, and that is structural rather
/// than tidy.** The failure this seam exists to prevent is a listener that opens
/// in cleartext because TLS configuration failed. A `Server::builder()` call
/// anywhere else would be a place that downgrade could be written; with one, the
/// only way to reintroduce it is to add a fallback here, where
/// `a_tls_listener_refuses_a_cleartext_client` is looking.
///
/// `None` is the cleartext listener this module has always opened. `Some` is the
/// same server with an identity, and it returns an error rather than a cleartext
/// server if that identity is unusable.
///
/// **ALPN is tonic's, not ours.** `ServerTlsConfig` pushes `h2` onto the
/// acceptor's protocol list, and a gRPC listener that negotiated anything else
/// would answer nothing useful. It is verified rather than assumed: tonic's own
/// client refuses a channel whose negotiated protocol is not `h2`, so the
/// handshake cases in `tests/serve_tls.rs` fail if it ever stops being offered.
///
/// **Called BEFORE the probe and the migration**, so that a deployment which
/// asked for TLS and got the mount wrong exits without touching the engine at
/// all. D69 puts the refusals first; this one is cheaper than the rest.
pub fn server(tls: Option<&ServeTls>) -> Result<Server, BootError> {
    let server = Server::builder();
    let Some(tls) = tls else {
        return Ok(server);
    };

    let identity = tls.identity()?;
    // EAGER, and before anything binds. `tls_config` builds the rustls acceptor
    // here — it is what decodes the PEM and checks that the certificate belongs
    // to the key — so a bad pair is an error at boot rather than a handshake
    // that fails on a stranger's first connection.
    server
        .tls_config(ServerTlsConfig::new().identity(identity))
        .map_err(|e| BootError::TlsUnusable {
            cert: tls.cert_file.clone(),
            key: tls.key_file.clone(),
            detail: describe(&e),
        })
}

/// Flatten an error and everything under it into one sentence.
///
/// `tonic::transport::Error` displays as "transport error" and keeps what
/// actually went wrong in its source — so the message an operator needs is the
/// CHAIN, not the head of it. Losing it is the same class of mistake as printing
/// `Debug` from `main`.
fn describe(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        out.push_str(": ");
        out.push_str(&next.to_string());
        source = next.source();
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error(
        "DB_REQUIRE_TLS is set and this binary no longer reads it. Set DB_SSL_MODE \
         instead — one of: disabled, preferred, required, verify_ca, verify_identity \
         (default: required). Refusing at boot rather than ignoring the key, because \
         an operator who set it is asking for a transport guarantee, and silently \
         substituting a default is the one outcome worse than stopping. \
         DB_REQUIRE_TLS was a boolean and could not ask for certificate \
         verification at all; verify_ca and verify_identity are why it is gone."
    )]
    ObsoleteRequireTls,

    #[error(
        "{0}_TLS_ENABLED is set but {0}_TLS_CERT_FILE names no certificate. TLS was \
         asked for, so this is a deployment mistake rather than a reason to open a \
         plaintext listener — and it is NOT the same as leaving TLS off, which is the \
         supported way to serve without one. Point {0}_TLS_CERT_FILE at the PEM \
         certificate this module should present."
    )]
    NoTlsCertFile(&'static str),

    #[error(
        "{0}_TLS_ENABLED is set but {0}_TLS_KEY_FILE names no private key. A \
         certificate without its key cannot complete a handshake, so this refuses \
         rather than opening a plaintext listener. Point {0}_TLS_KEY_FILE at the PEM \
         private key belonging to {0}_TLS_CERT_FILE."
    )]
    NoTlsKeyFile(&'static str),

    #[error(
        "the TLS {what} at {path} could not be read: {source}. TLS was asked for, so \
         this module refuses to start rather than serving in cleartext. The usual \
         cause is a Secret that was never mounted, or a key inside it under a \
         different name than the chart selected."
    )]
    TlsUnreadable {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "the TLS certificate at {cert} and the private key at {key} were read but \
         refused: {detail}. Both files exist, so this is their CONTENT: a PEM that \
         decodes to no certificate at all, or a certificate that does not belong to \
         the key beside it — what a half-finished rotation leaves behind. This module \
         refuses to start rather than serving in cleartext."
    )]
    TlsUnusable {
        cert: PathBuf,
        key: PathBuf,
        detail: String,
    },

    #[error(transparent)]
    Pool(#[from] PoolError),

    #[error(transparent)]
    Int(#[from] std::num::ParseIntError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use yadgar_store::pool::MySqlSslMode;

    /// An environment stating only what a test cares about.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    /// `verify_identity` throughout, and never `required`.
    ///
    /// `required` is what this module defaults to and what the old boolean
    /// selected, so an implementation that ignored the configuration entirely
    /// would pass a test fixed on it. `verify_identity` is a mode neither this
    /// code nor sqlx would ever arrive at on its own.
    fn config_with(mode: MySqlSslMode) -> PoolConfig {
        PoolConfig {
            host: "engine.example.invalid".to_string(),
            port: 13306,
            database: "task_fixture".to_string(),
            username: "task_fixture_user".to_string(),
            max_connections: 4,
            replicas: 2,
            engine_max_connections: 151,
            ssl_mode: mode,
        }
    }

    #[test]
    fn the_probe_connects_with_the_configured_mode_not_a_mode_of_its_own() {
        let config = config_with(MySqlSslMode::VerifyIdentity);
        let options = probe_connect_options(&config, &Secret::new("fixture".to_string()));

        // The whole defect was a probe that described its connection itself. Had
        // it kept doing so — hardcoding Required, or falling back to sqlx's
        // Preferred — this is the assertion that would not hold.
        assert!(
            matches!(options.get_ssl_mode(), MySqlSslMode::VerifyIdentity),
            "the probe did not take the configured ssl-mode"
        );

        // The rest of the connection comes from the same place, so a probe
        // pointed at a different host or database would be caught here too.
        // NO DSN in any message: an assert that interpolates one puts a username
        // into CI output.
        assert_eq!(options.get_host(), "engine.example.invalid");
        assert_eq!(options.get_port(), 13306);
        assert_eq!(options.get_username(), "task_fixture_user");
        assert_eq!(options.get_database(), Some("task_fixture"));
    }

    #[test]
    fn a_set_but_obsolete_db_require_tls_refuses_the_boot() {
        // `true` deliberately: the value an operator sets to ASK for TLS. Under
        // the old expression it was the one spelling that worked, so it is the
        // value most likely to be sitting in a deployment right now — and the
        // one whose silent removal changes nothing visible while removing the
        // guarantee the operator wrote down.
        let err = pool_config(env_of(&[("DB_REQUIRE_TLS", "true")]))
            .expect_err("a set DB_REQUIRE_TLS must refuse the boot");

        assert!(matches!(err, BootError::ObsoleteRequireTls));
        let message = err.to_string();
        assert!(message.contains("DB_SSL_MODE"), "{message}");
    }

    #[test]
    fn the_obsolete_key_is_refused_before_any_other_value_is_parsed() {
        // The refusal must win against a second, unrelated fault. Otherwise the
        // operator fixes the port, boots, and never learns the key is inert.
        let err = pool_config(env_of(&[
            ("DB_REQUIRE_TLS", "true"),
            ("DB_PORT", "not-a-port"),
        ]))
        .expect_err("must refuse");

        assert!(matches!(err, BootError::ObsoleteRequireTls), "{err}");
    }

    #[test]
    fn certificate_verification_is_reachable_from_the_environment() {
        // The reason the boolean had to go: no value of DB_REQUIRE_TLS could
        // ask the engine to prove who it is. The hyphen spelling is the one a
        // chart writes; sqlx writes the underscore.
        let config = pool_config(env_of(&[("DB_SSL_MODE", "verify-identity")])).expect("config");
        assert!(matches!(config.ssl_mode, MySqlSslMode::VerifyIdentity));

        let config = pool_config(env_of(&[("DB_SSL_MODE", "VERIFY_CA")])).expect("config");
        assert!(matches!(config.ssl_mode, MySqlSslMode::VerifyCa));
    }

    #[test]
    fn an_unrecognised_ssl_mode_refuses_the_boot_rather_than_falling_back() {
        // `yes` is not arbitrary. Under the expression this replaces —
        // `env_or("DB_REQUIRE_TLS", "true") == "true"` — it evaluated FALSE and
        // selected an unencrypted connection, silently. Failing open on a
        // transport question is the class of bug, not one spelling of it.
        let err = pool_config(env_of(&[("DB_SSL_MODE", "yes")]))
            .expect_err("an unrecognised mode must refuse the boot");

        assert!(
            matches!(err, BootError::Pool(PoolError::UnknownSslMode { .. })),
            "{err}"
        );
    }

    /// SENTINELS for the listener's configuration: nothing in this module could
    /// produce either path, so a test that sees one saw it travel from the
    /// lookup.
    const SENTINEL_CERT: &str = "/etc/yadgar/pangolin-7c21/serving.crt";
    const SENTINEL_KEY: &str = "/etc/yadgar/pangolin-7c21/serving.key";

    /// THE DEFAULT for the listener, and the property the whole change is built
    /// around: nothing configured means the cleartext listener, unchanged.
    #[test]
    fn nothing_configured_means_the_listener_serves_cleartext() {
        assert_eq!(ServeTls::from_lookup(SERVE, env_of(&[])).unwrap(), None);
    }

    /// A certificate without the flag is the REVERTED state, not an error. The
    /// flag is the lever; leaving the paths in place is how it gets pulled back.
    #[test]
    fn a_certificate_alone_does_not_enable_the_listeners_tls() {
        let vars = [
            ("SERVE_TLS_CERT_FILE", SENTINEL_CERT),
            ("SERVE_TLS_KEY_FILE", SENTINEL_KEY),
        ];
        assert_eq!(ServeTls::from_lookup(SERVE, env_of(&vars)).unwrap(), None);
    }

    /// Anything but "1" is off — the same parse `DB_SSL_MODE`'s predecessor got
    /// wrong, and the reason it is spelled out rather than inferred.
    #[test]
    fn only_exactly_one_enables_the_listeners_tls() {
        for value in ["0", "false", "no", "true", "yes", "", " "] {
            let vars = [
                ("SERVE_TLS_ENABLED", value),
                ("SERVE_TLS_CERT_FILE", SENTINEL_CERT),
                ("SERVE_TLS_KEY_FILE", SENTINEL_KEY),
            ];
            assert_eq!(
                ServeTls::from_lookup(SERVE, env_of(&vars)).unwrap(),
                None,
                "{value:?} must not enable TLS"
            );
        }
    }

    /// THE FAILURE THAT MUST NOT DEGRADE. Asking for TLS and naming neither file
    /// is a deployment mistake, and the answer to it is an error rather than a
    /// plaintext listener. The message names the half that is missing.
    #[test]
    fn asking_the_listener_for_tls_without_the_files_is_an_error() {
        let missing_cert = [
            ("SERVE_TLS_ENABLED", "1"),
            ("SERVE_TLS_KEY_FILE", SENTINEL_KEY),
        ];
        assert!(
            matches!(
                ServeTls::from_lookup(SERVE, env_of(&missing_cert)),
                Err(BootError::NoTlsCertFile("SERVE"))
            ),
            "a missing certificate must be refused, not silently downgraded"
        );

        let missing_key = [
            ("SERVE_TLS_ENABLED", "1"),
            ("SERVE_TLS_CERT_FILE", SENTINEL_CERT),
        ];
        assert!(
            matches!(
                ServeTls::from_lookup(SERVE, env_of(&missing_key)),
                Err(BootError::NoTlsKeyFile("SERVE"))
            ),
            "a missing private key must be refused, not silently downgraded"
        );
    }

    /// Both paths reach the settings, proved with names the module could not
    /// have chosen for itself.
    #[test]
    fn the_certificate_and_the_key_both_arrive() {
        let vars = [
            ("SERVE_TLS_ENABLED", "1"),
            ("SERVE_TLS_CERT_FILE", SENTINEL_CERT),
            ("SERVE_TLS_KEY_FILE", SENTINEL_KEY),
        ];
        let tls = ServeTls::from_lookup(SERVE, env_of(&vars))
            .unwrap()
            .expect("a flag, a certificate and a key enable TLS");
        assert_eq!(tls.cert_file(), Path::new(SENTINEL_CERT));
        assert_eq!(tls.key_file(), Path::new(SENTINEL_KEY));
    }

    /// The two directions cannot configure each other. `DB_SSL_MODE` decides how
    /// this module reaches its ENGINE and says nothing about what it serves, and
    /// a bare `TLS_ENABLED` belongs to neither.
    #[test]
    fn the_engines_transport_does_not_configure_the_listener() {
        let vars = [
            ("DB_SSL_MODE", "verify-identity"),
            ("TLS_ENABLED", "1"),
            ("TLS_CERT_FILE", SENTINEL_CERT),
        ];
        assert_eq!(ServeTls::from_lookup(SERVE, env_of(&vars)).unwrap(), None);
    }

    #[test]
    fn the_default_encrypts_and_does_not_fall_back() {
        // An empty environment is the shipped deployment. `Preferred` here would
        // mean the fix reintroduced the defect through the default.
        let config = pool_config(env_of(&[])).expect("config");
        assert!(
            matches!(config.ssl_mode, MySqlSslMode::Required),
            "the default ssl-mode must encrypt without falling back"
        );
    }
}
