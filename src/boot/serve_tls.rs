//! The transport this module SERVES on — wired, not implemented, here.
//!
//! **THE IMPLEMENTATION IS `yadgar_lifecycle::serve_tls` (ADR-0846, card
//! B-U5).** The local `ServeTls` copy this file held is deleted: six gRPC
//! servers each carried one, none called `client_ca_root`, and so no hop in the
//! estate verified a client certificate. The lifted [`ServerTls`] reads the
//! same `LISTEN_TLS_*` keys, refuses an absent `LISTEN_TLS_ENABLED` and an
//! absent `LISTEN_TLS_CLIENT_AUTH` alike (ADR-0845, ADR-0854), and verifies a
//! caller's certificate against `LISTEN_TLS_CLIENT_CA_FILE` when the mode is
//! `optional` or `required`.
//!
//! What stays in this repository is the WIRING: the prefix, the chart key the
//! refusals name, and the one sentence `main` prints for a refusal. Each is a
//! single call, so `main` and `tests/serve_tls.rs` reach the listener through
//! exactly the same path.

use tonic::transport::Server;
pub use yadgar_lifecycle::serve_tls::{ClientAuth, ServeTlsError, ServerTls, LISTEN};
// THE ONE ERROR-CHAIN FLATTENER FOR THE ESTATE (ADR-0591).
use yadgar_telemetry::diagnose::chain;

/// The values block this module's listener keys render from. Every refusal
/// names a key under it (`tls.enabled`, `tls.clientAuth`, `tls.certSecret`,
/// `tls.clientCaSecret`), so an operator reading a crash loop knows which
/// chart value to edit.
pub const LISTEN_CHART_KEY: &str = "tls";

/// Read the listener's transport through `lookup`: `LISTEN_TLS_*`, named for
/// `tls.*` in every refusal.
///
/// `Ok(None)` is the cleartext listener, and only an EXPLICIT
/// `LISTEN_TLS_ENABLED=0` with `LISTEN_TLS_CLIENT_AUTH=off` produces it.
/// `main` passes `std::env::var`; the lookup is injected because `std::env`
/// is process-global, so a test that set one variable would steer every other
/// test in the binary.
pub fn listener(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<ServerTls>, ServeTlsError> {
    ServerTls::from_lookup(LISTEN, LISTEN_CHART_KEY, lookup)
}

/// Build the gRPC server this module listens with.
///
/// **THE ONLY SERVER CONSTRUCTION IN THIS BINARY.** The failure this seam
/// exists to prevent is a listener that opens in cleartext because TLS
/// configuration failed; with one construction site, the only way to write
/// that fallback is here, where `a_tls_listener_refuses_a_cleartext_client`
/// is looking. `None` is cleartext; `Some` is TLS or an error, never
/// cleartext.
///
/// **EAGER, and called BEFORE the probe and the migration**: lifecycle builds
/// the rustls acceptor here — the PEM decoded, the certificate matched to its
/// key, the client CA parsed — so a bad mount exits without touching the
/// engine. D69 puts the refusals first.
pub fn server(tls: Option<&ServerTls>) -> Result<Server, ServeTlsError> {
    yadgar_lifecycle::serve_tls::server(tls)
}

/// The sentence `main` prints for a listener refusal: the WHOLE source chain,
/// through the estate's one flattener (ADR-0591).
///
/// lifecycle keeps tonic's error as `Unusable`'s SOURCE, and tonic's own
/// `Display` is the two words `transport error` — so the reason (rustls's
/// `keys may not be consistent`, say) is one `source()` hop down, and a bare
/// `to_string()` would print none of it. `Unreadable` formats its `io::Error`
/// inline and also marks it `#[source]`, so its reason appears twice; a
/// repeated reason is the cheaper failure than a missing one.
pub fn refusal(error: &ServeTlsError) -> String {
    chain(error)
}
