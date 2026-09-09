//! The transport this module SERVES on: the identity it presents, and the one
//! gRPC server built from it.
//!
//! Split out of `boot.rs` rather than rehomed. Every line below stood in that
//! file unchanged, and the seam it is cut along is the one `tests/serve_tls.rs`
//! already names — the LISTENER's transport, as against the connection OUT to
//! the engine that the rest of `boot` configures. `LISTEN` travels with it
//! because it is the prefix those keys are built from and nothing else reads it.

use std::path::{Path, PathBuf};

use tonic::transport::{Identity, Server, ServerTlsConfig};
// THE ONE ERROR-CHAIN FLATTENER FOR THE ESTATE (ADR-0591). The body that used to
// sit above `BootError` in `boot.rs` was one of five — `iam`, `iam-db`, `task`,
// `project-db` and here — byte-identical apart from local names, under TWO
// names: `chain` in the first two and `describe` in the other three. It is
// deleted rather than left beside the shared one, because a consolidation that
// adds a sixth copy without removing the five is worse than none.
use yadgar_telemetry::diagnose::chain;

use super::BootError;

/// The environment variables this module's own listener is configured from:
/// `LISTEN_TLS_ENABLED`, `LISTEN_TLS_CERT_FILE` and `LISTEN_TLS_KEY_FILE`.
///
/// Built from a PREFIX rather than written out three times, so the naming stays
/// mechanical.
///
/// **THE PREFIX NAMES THE THING BEING CONFIGURED, and it is derived rather than
/// chosen.** `LISTEN` is already the variable holding the address this module
/// binds, so the listener's transport keys extend a name that exists. A
/// connection OUT is named for what it reaches, which is why `DB_*` means the
/// engine. `SERVE` — what this constant used to be — invented a second word for
/// the listener, and so named nothing the process otherwise had. `iam` and
/// `iam-db` derived `LISTEN` independently for the identical seam; one idea
/// spelled two ways across the estate is its own defect, and `task` now reads
/// the same prefix for the same reason.
///
/// A bare `TLS_ENABLED` is ambiguous between the two directions, which is what
/// makes a prefix necessary at all — `the_engines_transport_does_not_configure_the_listener`
/// pins that half.
pub const LISTEN: &str = "LISTEN";

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
    // ADR-0523-WATCHED: ServeTls
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
            detail: chain(&e),
        })
}
