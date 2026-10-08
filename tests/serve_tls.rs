//! The serving side of the transport, proved by real handshakes.
//!
//! **A test that only shows "TLS was configured" passes against the broken
//! version of this change**, so nothing here inspects configuration. Every case
//! stands up this module's own listener through [`yadgar_task_db::boot::listener`]
//! and [`yadgar_task_db::boot::server`] — the two calls `main` builds its gRPC
//! server with — and asks whether a request survived the transport.
//!
//! **THE TRANSPORT IS `yadgar_lifecycle::serve_tls` NOW (ADR-0846, card B-U5).**
//! The local `ServeTls` copy is deleted; what stays here is this module's own
//! WIRING — the prefix, the chart key and the one server construction — proved
//! through the same calls `main` makes, including the client-certificate modes
//! (ADR-0854) the copy never had.
//!
//! THE DEFECT THIS CAR EXISTS TO REMOVE is a listener that opens in cleartext
//! because TLS configuration failed. `a_tls_listener_refuses_a_cleartext_client`
//! is the case that notices: it is the only one that fails if `server` ever
//! answers a bad identity — or a good one — with a plain `Server::builder()`.
//!
//! NEITHER AN ENGINE NOR A POOL IS INVOLVED. `boot::server` decides a transport
//! and nothing else, so these cases run with no `YADGAR_TEST_DSN` and no
//! MariaDB — unlike the suites that exercise the store.
//!
//! ALPN IS PROVED RATHER THAN ASSUMED, and by tonic's client rather than by an
//! assertion written here. `tonic::transport::Endpoint` errors when the
//! negotiated protocol is not `h2` and `assume_http2` is unset, which it is by
//! default — so a channel that connects and carries a request has negotiated
//! `h2`. The push of `h2` onto the acceptor's protocol list is tonic's own; there
//! is no line in this repository to mutate, and saying so is more honest than
//! dressing it as coverage.
//!
//! CERTIFICATES ARE MINTED PER RUN. A fixture key committed to the repository is
//! a secret committed to the repository, and it expires on a date nobody is
//! watching.
//!
//! NOTE ON `localhost`: on this machine it resolves to BOTH `::1` and
//! `127.0.0.1`, so a rig that binds one of them is flaky by construction — the
//! client picks an address the server is not on. [`serve_on_localhost`] binds
//! every address the name resolves to, on one port. That is a property of the
//! rig, not of the module.

use std::collections::HashSet;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, OnceLock};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::codegen::{http, Service};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use yadgar_task_db::boot::{self, ServeTlsError, ServerTls};

/// The name the test certificates are issued for, and the name the rig listens
/// on.
const SERVED_NAME: &str = "localhost";

/// A certificate authority, the server certificate it issued, and a CLIENT
/// certificate it issued — the one a caller presents to a verifying listener.
struct Pki {
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

/// Mint a CA, a server certificate for `san`, and a client certificate.
///
/// **THE CLIENT LEAF CARRIES `clientAuth`, the server leaf `serverAuth` only.**
/// A verifying listener refuses a leaf that names an EKU and omits
/// `clientAuth`, so presenting the server's own certificate as a client one
/// would be refused for that reason rather than the one a case asks about.
fn pki(san: &str) -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "yadgar-task-db test authority");
    let ca = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();

    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![san.to_string()]).unwrap();
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.distinguished_name.push(DnType::CommonName, san);
    let cert = params.signed_by(&key, &ca).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    client_params
        .distinguished_name
        .push(DnType::CommonName, "yadgar-task test caller");
    let client_cert = client_params.signed_by(&client_key, &ca).unwrap();

    Pki {
        ca_pem: ca.pem(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        client_cert_pem: client_cert.pem(),
        client_key_pem: client_key.serialize_pem(),
    }
}

/// A file that deletes itself, so a certificate and a key can be handed over as
/// PATHS — which is the only shape [`ServerTls`] accepts, and the reason it
/// accepts it (D80).
struct TempPem(PathBuf);

/// One reading of the clock per PROCESS, so two runs that the OS gave the same
/// recycled pid do not name the same files. It varies per run and never within
/// one, which is what leaves [`unique_name`] with exactly one varying part.
fn run_id() -> u128 {
    static RUN: OnceLock<u128> = OnceLock::new();
    *RUN.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    })
}

/// The name of one temporary PEM, unique within this process by CONSTRUCTION.
///
/// **THE CLOCK IS NOT A UNIQUENESS SOURCE ACROSS THREADS, and this is measured
/// on this tree rather than assumed.** The name used to be `pid` plus a fresh
/// nanosecond reading. Every test in this binary shares the pid and they run on
/// threads, so two concurrent calls collide whenever both readings land on the
/// same nanosecond — and then one `TempPem`'s `Drop` deletes a path a sibling
/// test is still reading. A clock is a timestamp, not a nonce (ledger 629).
///
/// The counter is the ONLY part that varies within a run, which is what makes
/// the property assertable rather than merely likely.
fn unique_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "yadgar-task-db-{}-{}-{}.pem",
        std::process::id(),
        run_id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The SEQUENTIAL property, and it is a mutation guard rather than a
/// reproduction — stated plainly because the distinction was measured. It
/// PASSES against the clock-based name this change replaces: same-thread
/// readings advance by tens of nanoseconds and never repeat, so a sequential
/// assertion cannot see the defect.
///
/// MUTATION: replace `fetch_add(1, ..)` with `load(..)` and this fails on every
/// run.
#[test]
fn two_temporary_names_are_never_the_same_name() {
    assert_ne!(unique_name(), unique_name());

    let many: HashSet<String> = (0..1000).map(|_| unique_name()).collect();
    assert_eq!(many.len(), 1000, "1000 names must be 1000 distinct names");
}

/// THE CONCURRENT PROPERTY, which is the one that reproduces the defect. This
/// is the failing test the fix was written against: run against the clock-based
/// name on this machine it reported **3731 of 32000 names collided across 16
/// threads**, which is the whole mechanism behind ledger 629 in one assertion.
/// Cross-thread readings of `SystemTime::now()` repeat constantly; same-thread
/// ones do not.
#[test]
fn concurrent_names_are_all_distinct() {
    const THREADS: usize = 16;
    const PER_THREAD: usize = 2000;

    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                (0..PER_THREAD).map(|_| unique_name()).collect::<Vec<_>>()
            })
        })
        .collect();

    let all: Vec<String> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let distinct: HashSet<&String> = all.iter().collect();
    assert_eq!(
        distinct.len(),
        THREADS * PER_THREAD,
        "{} of {} names collided across {THREADS} threads",
        THREADS * PER_THREAD - distinct.len(),
        THREADS * PER_THREAD
    );
}

impl TempPem {
    fn with(contents: &str) -> Self {
        let path = std::env::temp_dir().join(unique_name());
        // `create_new`, not `fs::write`. Silence is what made the old collision
        // expensive: two tests shared a path, one deleted it, and the other
        // failed somewhere else entirely — as an unreadable file (`NotFound`) in 48
        // of 68 measured failures. If a name is ever reused, this panics and
        // names the file instead.
        let mut file = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("{} already exists or cannot be made: {e}", path.display()));
        file.write_all(contents.as_bytes()).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPem {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build a [`ServerTls`] through the SAME call `main` reads the environment
/// with — [`boot::listener`], which fixes the prefix and the chart key — so
/// these cases exercise the shipped path rather than a constructor built for
/// them. Client auth `off`: the listener verifies no caller.
fn serve_tls(cert: &Path, key: &Path) -> ServerTls {
    verifying(cert, key, "off", None)
}

/// The same, with a client-auth MODE and, for a verifying mode, the CA file
/// the caller's certificate is checked against.
fn verifying(cert: &Path, key: &Path, mode: &str, client_ca: Option<&Path>) -> ServerTls {
    let vars = [
        ("LISTEN_TLS_ENABLED", "1".to_string()),
        ("LISTEN_TLS_CERT_FILE", cert.display().to_string()),
        ("LISTEN_TLS_KEY_FILE", key.display().to_string()),
        ("LISTEN_TLS_CLIENT_AUTH", mode.to_string()),
    ];
    let ca = client_ca.map(|p| p.display().to_string());
    boot::listener(|k| {
        if k == "LISTEN_TLS_CLIENT_CA_FILE" {
            return ca.clone();
        }
        vars.iter()
            .find(|(name, _)| *name == k)
            .map(|(_, v)| v.clone())
    })
    .expect("a certificate, a key and a mode configure the listener")
    .expect("the flag is set")
}

/// Serve gRPC on every address `SERVED_NAME` resolves to, and return the shared
/// port. `Routes::default()` answers every method with `Unimplemented`, which is
/// the whole of what these cases need: the question each asks is whether a
/// request reached the server at all.
async fn serve_on_localhost(tls: Option<&ServerTls>) -> u16 {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((SERVED_NAME, 0))
        .await
        .unwrap()
        .collect();
    assert!(!addrs.is_empty(), "{SERVED_NAME} resolved to nothing");

    let (port, listeners) = bind_one_port_on_every_address(&addrs).await;
    for listener in listeners {
        spawn(listener, tls);
    }

    ready(port).await;
    port
}

/// How many ports to try before giving up on finding one free everywhere. The
/// count is NAMED rather than inlined so the panic below can state it; the
/// number is unchanged, because lowering it is a behaviour change with no
/// measurement behind it.
const BIND_ATTEMPTS: usize = 50;

/// One ephemeral port, bound on EVERY address the name resolves to.
///
/// **THE KERNEL PICKS THE PORT FOR ONE ADDRESS AND PROMISES NOTHING ABOUT THE
/// OTHERS.** Asking for an ephemeral port on `127.0.0.1` and then demanding that
/// same number on `::1` fails whenever a concurrently running test in this
/// binary was handed it there first — `EADDRINUSE` on the second bind, which the
/// old code turned into `.expect(..)`. Measured on unmodified `main`: 5 of 68
/// failing runs in 2000 were this panic rather than the name collision, so
/// fixing only the filed defect would have left the suite flaky.
///
/// So the whole SET is acquired before anything is spawned, and a partial
/// acquisition is dropped and retried with a fresh port. Retrying is honest
/// here: the failure is another process holding a number, which the next number
/// does not have.
///
/// **RETRYING IS ONLY HONEST FOR A PORT SOMEBODY ELSE HOLDS.** Every other bind
/// error is permanent, so retrying one spends fifty ports to learn nothing and
/// then blames port exhaustion for it. The case that makes this concrete: on a
/// host with IPv6 disabled where `localhost` still resolves `::1`, every bind on
/// `::1` returns `EADDRNOTAVAIL`. So `AddrInUse` is retried and every other
/// error names the address it happened on — the form `yadgar-dial`'s
/// `tests/common/mod.rs` already carries, which this rig cited as its precedent
/// and then did not adopt (ledger 708).
async fn bind_one_port_on_every_address(addrs: &[SocketAddr]) -> (u16, Vec<TcpListener>) {
    for _ in 0..BIND_ATTEMPTS {
        let first = TcpListener::bind(addrs[0])
            .await
            .unwrap_or_else(|e| panic!("no free port on {}: {e}", addrs[0].ip()));
        let port = first.local_addr().unwrap().port();

        let mut listeners = vec![first];
        for addr in &addrs[1..] {
            match TcpListener::bind(SocketAddr::new(addr.ip(), port)).await {
                Ok(listener) => listeners.push(listener),
                // Dropping `listeners` releases the port on every address it was
                // taken on, so the next attempt starts from nothing held.
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => break,
                Err(e) => panic!("binding {} on port {port}: {e}", addr.ip()),
            }
        }
        if listeners.len() == addrs.len() {
            return (port, listeners);
        }
    }
    panic!(
        "no ephemeral port was free on all {} addresses in {BIND_ATTEMPTS} attempts",
        addrs.len()
    );
}

/// A PERMANENT bind failure on a LATER address names that address, rather than
/// being spent as one of [`BIND_ATTEMPTS`] retries.
///
/// The failure is a real one rather than a mocked one: `192.0.2.0/24` is
/// TEST-NET-1, reserved for documentation and assigned to no interface, so
/// binding it returns `EADDRNOTAVAIL`. Against the `Err(_) => break` this
/// replaces, the case panicked with "no ephemeral port was free on all 2
/// addresses" — port exhaustion, which is the wrong diagnosis and the whole of
/// ledger 708.
#[tokio::test]
#[should_panic(expected = "binding 192.0.2.1")]
async fn a_permanent_failure_on_a_later_address_is_reported_not_retried() {
    let addrs = [
        SocketAddr::from(([127, 0, 0, 1], 0)),
        SocketAddr::from(([192, 0, 2, 1], 0)),
    ];
    bind_one_port_on_every_address(&addrs).await;
}

/// The same property on the FIRST address, which is a SEPARATE path through the
/// same function and the likelier one to meet a disabled address family:
/// `localhost` resolves `::1` AHEAD of `127.0.0.1` on this machine, so a host
/// with IPv6 off fails on `addrs[0]` before the loop is ever reached. That bind
/// used to carry `.expect("an ephemeral port on the first address")`, which
/// named no address and reported the wrong cause just as the loop did.
#[tokio::test]
#[should_panic(expected = "no free port on 192.0.2.1")]
async fn a_permanent_failure_on_the_first_address_is_reported_not_retried() {
    let addrs = [SocketAddr::from(([192, 0, 2, 1], 0))];
    bind_one_port_on_every_address(&addrs).await;
}

fn spawn(listener: TcpListener, tls: Option<&ServerTls>) {
    let mut builder = boot::server(tls).expect("a usable identity");
    let router = builder.add_routes(tonic::service::Routes::default());
    tokio::spawn(async move {
        let _ = router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
    });
}

/// Wait until the port accepts a TCP connection, rather than sleeping a guessed
/// interval.
async fn ready(port: u16) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect((SERVED_NAME, port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the test server never accepted a connection on port {port}");
}

/// Send one gRPC request down the channel and report the HTTP status it came
/// back with.
///
/// `Ok(200)` means the transport carried it: the handshake completed and the
/// server answered — with `Unimplemented`, which is a perfectly good answer to
/// this question. `Err` means it never got there.
async fn request(mut channel: Channel) -> Result<u16, String> {
    let req = http::Request::builder()
        .version(http::Version::HTTP_2)
        .method("POST")
        .uri(format!(
            "https://{SERVED_NAME}/yadgar.task.v1.TaskDbService/Probe"
        ))
        .header("content-type", "application/grpc")
        .body(tonic::body::Body::empty())
        .unwrap();

    std::future::poll_fn(|cx| channel.poll_ready(cx))
        .await
        .map_err(|e| format!("{e}"))?;
    match tokio::time::timeout(Duration::from_secs(10), channel.call(req)).await {
        Err(_) => Err("the request timed out".to_string()),
        Ok(Ok(response)) => Ok(response.status().as_u16()),
        Ok(Err(e)) => Err(format!("{e}")),
    }
}

/// Reach the listener the way a TLS client does, verifying against `ca_pem`.
async fn over_tls(port: u16, ca_pem: &str) -> Result<u16, String> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .domain_name(SERVED_NAME);
    dial(port, tls).await
}

/// The same, PRESENTING a client certificate — what `task` does once its hop
/// is verified (ADR-0852).
async fn over_mtls(port: u16, ca_pem: &str, cert_pem: &str, key_pem: &str) -> Result<u16, String> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .identity(Identity::from_pem(cert_pem, key_pem))
        .domain_name(SERVED_NAME);
    dial(port, tls).await
}

/// Connect with `tls` and send one request. A refused client certificate can
/// surface at `connect()` or — under TLS 1.3, where the server's verdict
/// follows the client's Finished — at the first request, so the outcome is
/// read at the REQUEST, the same place lifecycle's own matrix reads it.
async fn dial(port: u16, tls: ClientTlsConfig) -> Result<u16, String> {
    let channel = Endpoint::from_shared(format!("https://{SERVED_NAME}:{port}"))
        .unwrap()
        .tls_config(tls)
        .map_err(|e| format!("{e}"))?
        .connect()
        .await
        .map_err(|e| format!("{e}"))?;
    request(channel).await
}

/// Reach the listener the way every caller in the estate does today.
async fn in_cleartext(port: u16) -> Result<u16, String> {
    let channel = Endpoint::from_shared(format!("http://{SERVED_NAME}:{port}"))
        .unwrap()
        .connect()
        .await
        .map_err(|e| format!("{e}"))?;
    request(channel).await
}

/// THE POINT OF THE CAR: a client speaking TLS gets an answer, so the hop that
/// carries every task body can be encrypted at all. HTTP 200 with a gRPC
/// `Unimplemented` in it is what a completed handshake plus a negotiated `h2`
/// looks like.
#[tokio::test]
async fn a_tls_listener_answers_a_tls_client() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let port = serve_on_localhost(Some(&serve_tls(cert.path(), key.path()))).await;

    assert_eq!(over_tls(port, &p.ca_pem).await, Ok(200));
}

/// THE SILENT DOWNGRADE, in the only form a test can see it. Every other case
/// here would still pass against a `server` that answered an unusable identity
/// — or a usable one — with a plain `Server::builder()`; this is the one that
/// would not, because a cleartext client would then be answered.
#[tokio::test]
async fn a_tls_listener_refuses_a_cleartext_client() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let port = serve_on_localhost(Some(&serve_tls(cert.path(), key.path()))).await;

    let outcome = in_cleartext(port).await;
    assert!(
        outcome.is_err(),
        "a listener told to serve TLS must not answer a cleartext client: {outcome:?}"
    );
}

/// The identity SERVED is the one configured, not any certificate that parses.
/// A client trusting a different authority must be refused — which is what an
/// impostor's certificate would look like, and the property that makes
/// verification worth doing at all.
#[tokio::test]
async fn a_client_trusting_another_authority_is_refused() {
    let served = pki(SERVED_NAME);
    let cert = TempPem::with(&served.cert_pem);
    let key = TempPem::with(&served.key_pem);
    let port = serve_on_localhost(Some(&serve_tls(cert.path(), key.path()))).await;

    // A second authority, which issued nothing this listener holds.
    let stranger = pki(SERVED_NAME);
    let outcome = over_tls(port, &stranger.ca_pem).await;
    assert!(
        outcome.is_err(),
        "a certificate from an authority the client does not trust must be refused: {outcome:?}"
    );
}

/// CLEARTEXT IS A STATED CHOICE, NOT A DEFAULT (ADR-0845, ADR-0854): an
/// explicit `LISTEN_TLS_ENABLED=0` with client auth `off` is the one input
/// [`boot::listener`] answers with no transport, and that listener serves
/// exactly what this module served before TLS existed. It also stops the case
/// above from starting to pass because everything became TLS.
#[tokio::test]
async fn an_explicitly_cleartext_listener_still_serves_cleartext() {
    let tls = boot::listener(lookup(&[
        ("LISTEN_TLS_ENABLED", "0"),
        ("LISTEN_TLS_CLIENT_AUTH", "off"),
    ]))
    .expect("an explicit 0 with client auth off is a valid listener");
    assert!(
        tls.is_none(),
        "an explicit 0 must answer the cleartext listener"
    );
    let port = serve_on_localhost(tls.as_ref()).await;
    assert_eq!(in_cleartext(port).await, Ok(200));
}

/// A lookup over fixed pairs, for the configuration cases below.
fn lookup(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |key| {
        vars.iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_string())
    }
}

/// ADR-0845 THROUGH THIS MODULE'S WIRING: an absent `LISTEN_TLS_ENABLED`
/// refuses, naming the variable AND the chart value that renders it. The
/// sentence is lifecycle's; the prefix and the chart key are this module's,
/// which is why the case lives here.
#[test]
fn an_absent_listen_tls_enabled_refuses_naming_the_chart_key() {
    let error = boot::listener(lookup(&[("LISTEN_TLS_CLIENT_AUTH", "off")]))
        .expect_err("an absent LISTEN_TLS_ENABLED must refuse the boot");
    assert!(
        matches!(error, ServeTlsError::EnabledMissing { .. }),
        "an absent LISTEN_TLS_ENABLED must refuse as missing"
    );
    let message = boot::refusal(&error);
    assert!(
        message.contains("LISTEN_TLS_ENABLED"),
        "the refusal must name the variable"
    );
    assert!(
        message.contains("`tls.enabled`"),
        "the refusal must name the chart key"
    );
}

/// ADR-0854 THROUGH THIS MODULE'S WIRING: an absent `LISTEN_TLS_CLIENT_AUTH`
/// refuses whether or not TLS is on — deleting the variable is a boot
/// refusal, never a way to turn verification off. Both values of the switch
/// are tried, because the cleartext branch is the one a chart that rendered
/// the mode only under `tls.enabled` would hit.
#[test]
fn an_absent_listen_tls_client_auth_refuses_naming_the_chart_key() {
    let cases: [&'static [(&'static str, &'static str)]; 2] = [
        &[("LISTEN_TLS_ENABLED", "0")],
        &[
            ("LISTEN_TLS_ENABLED", "1"),
            ("LISTEN_TLS_CERT_FILE", "/nonexistent/tls.crt"),
            ("LISTEN_TLS_KEY_FILE", "/nonexistent/tls.key"),
        ],
    ];
    for vars in cases {
        let error = boot::listener(lookup(vars))
            .expect_err("an absent LISTEN_TLS_CLIENT_AUTH must refuse the boot");
        assert!(
            matches!(error, ServeTlsError::ClientAuthMissing { .. }),
            "an absent LISTEN_TLS_CLIENT_AUTH must refuse as missing"
        );
        let message = boot::refusal(&error);
        assert!(
            message.contains("LISTEN_TLS_CLIENT_AUTH"),
            "the refusal must name the variable"
        );
        assert!(
            message.contains("`tls.clientAuth`"),
            "the refusal must name the chart key"
        );
    }
}

/// A mode outside the three is refused, naming both keys — `on`, `true` and a
/// capitalised `Required` are how a typo becomes a posture.
#[test]
fn an_unknown_client_auth_mode_refuses_naming_the_chart_key() {
    for vars in [
        &[
            ("LISTEN_TLS_ENABLED", "0"),
            ("LISTEN_TLS_CLIENT_AUTH", "on"),
        ][..],
        &[
            ("LISTEN_TLS_ENABLED", "0"),
            ("LISTEN_TLS_CLIENT_AUTH", "Required"),
        ][..],
    ] {
        let error = boot::listener(lookup(vars)).expect_err("an unknown mode must refuse");
        assert!(
            matches!(error, ServeTlsError::ClientAuthInvalid { .. }),
            "an unknown mode must refuse as invalid"
        );
        let message = boot::refusal(&error);
        assert!(
            message.contains("LISTEN_TLS_CLIENT_AUTH"),
            "the refusal must name the variable"
        );
        assert!(
            message.contains("`tls.clientAuth`"),
            "the refusal must name the chart key"
        );
    }
}

/// `off` VERIFIES NOTHING: a TLS client presenting no certificate is served.
/// The emergency value, and the one the parent states on every server until
/// a hop is cut over (B-P1).
#[tokio::test]
async fn client_auth_off_serves_a_client_without_a_certificate() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let tls = serve_tls(cert.path(), key.path());
    assert!(
        tls.client_ca_file().is_none(),
        "`off` must read no client CA"
    );
    let port = serve_on_localhost(Some(&tls)).await;
    assert_eq!(over_tls(port, &p.ca_pem).await, Ok(200));
}

/// THE POINT OF THE CARD: `required` refuses a caller that presents no
/// certificate. Before B-U5 nothing in this module called `client_ca_root`,
/// so this request was answered.
#[tokio::test]
async fn client_auth_required_refuses_a_client_without_a_certificate() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let ca = TempPem::with(&p.ca_pem);
    let tls = verifying(cert.path(), key.path(), "required", Some(ca.path()));
    let port = serve_on_localhost(Some(&tls)).await;

    let outcome = over_tls(port, &p.ca_pem).await;
    assert!(
        outcome.is_err(),
        "`required` must refuse a caller that presents no certificate"
    );
}

/// ...and answers one whose certificate the configured authority signed. With
/// the case above, this is what tells "verifies" apart from "refuses
/// everything".
#[tokio::test]
async fn client_auth_required_accepts_a_client_the_ca_signed() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let ca = TempPem::with(&p.ca_pem);
    let tls = verifying(cert.path(), key.path(), "required", Some(ca.path()));
    let port = serve_on_localhost(Some(&tls)).await;

    let outcome = over_mtls(port, &p.ca_pem, &p.client_cert_pem, &p.client_key_pem).await;
    assert_eq!(outcome, Ok(200));
}

/// A client certificate from ANOTHER authority is refused: the listener checks
/// the anchor, not merely that some certificate arrived.
#[tokio::test]
async fn client_auth_required_refuses_a_client_another_ca_signed() {
    let p = pki(SERVED_NAME);
    let stranger = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let ca = TempPem::with(&p.ca_pem);
    let tls = verifying(cert.path(), key.path(), "required", Some(ca.path()));
    let port = serve_on_localhost(Some(&tls)).await;

    let outcome = over_mtls(
        port,
        &p.ca_pem,
        &stranger.client_cert_pem,
        &stranger.client_key_pem,
    )
    .await;
    assert!(
        outcome.is_err(),
        "`required` must refuse a client certificate another authority signed"
    );
}

/// `optional` serves a caller with no certificate — a staging step, not a
/// control — while still verifying one that is presented.
#[tokio::test]
async fn client_auth_optional_serves_no_certificate_and_verifies_a_presented_one() {
    let p = pki(SERVED_NAME);
    let stranger = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let key = TempPem::with(&p.key_pem);
    let ca = TempPem::with(&p.ca_pem);
    let tls = verifying(cert.path(), key.path(), "optional", Some(ca.path()));
    let port = serve_on_localhost(Some(&tls)).await;

    assert_eq!(over_tls(port, &p.ca_pem).await, Ok(200));
    let outcome = over_mtls(
        port,
        &p.ca_pem,
        &stranger.client_cert_pem,
        &stranger.client_key_pem,
    )
    .await;
    assert!(
        outcome.is_err(),
        "`optional` must still refuse a presented certificate that does not verify"
    );
}

/// A verifying mode with NO client CA refuses the boot, naming the variable
/// and the chart key that would supply it — rather than a listener that asks
/// for certificates it cannot check.
#[test]
fn a_verifying_mode_without_a_client_ca_refuses_naming_the_chart_key() {
    let error = boot::listener(lookup(&[
        ("LISTEN_TLS_ENABLED", "1"),
        ("LISTEN_TLS_CERT_FILE", "/nonexistent/tls.crt"),
        ("LISTEN_TLS_KEY_FILE", "/nonexistent/tls.key"),
        ("LISTEN_TLS_CLIENT_AUTH", "required"),
    ]))
    .expect_err("`required` with no client CA must refuse the boot");
    assert!(
        matches!(error, ServeTlsError::NoClientCaFile { .. }),
        "a verifying mode with no CA must refuse as a missing CA file"
    );
    let message = boot::refusal(&error);
    assert!(
        message.contains("LISTEN_TLS_CLIENT_CA_FILE"),
        "the refusal must name the variable"
    );
    assert!(
        message.contains("`tls.clientCaSecret`"),
        "the refusal must name the chart key"
    );
}

/// A path that is not there at all — the mistake an operator actually makes is a
/// mount that did not happen. The answer to it is an error naming the file, and
/// never a plaintext listener.
#[tokio::test]
async fn a_certificate_that_cannot_be_read_refuses_the_boot() {
    let p = pki(SERVED_NAME);
    let key = TempPem::with(&p.key_pem);
    let missing = std::env::temp_dir().join("yadgar-task-db-no-such-cert-31c7ae.pem");

    let tls = serve_tls(&missing, key.path());
    let error = boot::server(Some(&tls)).expect_err("a missing certificate must refuse");
    assert!(
        matches!(error, ServeTlsError::Unreadable { .. }),
        "a missing certificate must refuse as an unreadable file"
    );
    let message = boot::refusal(&error);
    assert!(
        message.contains("yadgar-task-db-no-such-cert-31c7ae.pem"),
        "the message must name the file that was wrong"
    );
}

/// The same for the private key, because naming the wrong half of the pair is
/// how an operator spends an afternoon on the wrong mount.
#[tokio::test]
async fn a_private_key_that_cannot_be_read_refuses_the_boot() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);
    let missing = std::env::temp_dir().join("yadgar-task-db-no-such-key-8de402.pem");

    let tls = serve_tls(cert.path(), &missing);
    let error = boot::server(Some(&tls)).expect_err("a missing private key must refuse");
    assert!(
        matches!(error, ServeTlsError::Unreadable { .. }),
        "a missing private key must refuse as an unreadable file"
    );
    let message = boot::refusal(&error);
    assert!(
        message.contains("yadgar-task-db-no-such-key-8de402.pem"),
        "the message must name the file that was wrong"
    );
}

/// A file that exists and holds no certificate. The PEM readers underneath
/// answer an EMPTY LIST rather than an error, so a file that decodes to nothing
/// can look like one that decoded fine — the shape the record says this failure
/// takes.
#[tokio::test]
async fn an_undecodable_certificate_refuses_the_boot() {
    let p = pki(SERVED_NAME);
    let key = TempPem::with(&p.key_pem);

    for contents in ["", "   ", "\n", "there is no certificate in this file\n"] {
        let cert = TempPem::with(contents);
        let tls = serve_tls(cert.path(), key.path());
        let outcome = boot::server(Some(&tls));
        assert!(
            matches!(outcome, Err(ServeTlsError::Unusable { .. })),
            "a certificate file containing {contents:?} must refuse the boot"
        );
    }
}

/// The same for a key that is not one.
#[tokio::test]
async fn an_undecodable_private_key_refuses_the_boot() {
    let p = pki(SERVED_NAME);
    let cert = TempPem::with(&p.cert_pem);

    for contents in ["", "   ", "\n", "there is no key in this file\n"] {
        let key = TempPem::with(contents);
        let tls = serve_tls(cert.path(), key.path());
        let outcome = boot::server(Some(&tls));
        assert!(
            matches!(outcome, Err(ServeTlsError::Unusable { .. })),
            "a key file containing {contents:?} must refuse the boot"
        );
    }
}

/// TWO VALID FILES THAT ARE NOT A PAIR. Both decode, so nothing about reading
/// them fails; the certificate simply does not belong to the key. It is what a
/// half-finished rotation leaves behind, and it must refuse rather than serve.
#[tokio::test]
async fn a_certificate_and_a_key_that_do_not_match_refuse_the_boot() {
    let served = pki(SERVED_NAME);
    let other = pki(SERVED_NAME);
    let cert = TempPem::with(&served.cert_pem);
    let key = TempPem::with(&other.key_pem);

    let tls = serve_tls(cert.path(), key.path());
    let outcome = boot::server(Some(&tls));
    assert!(
        matches!(outcome, Err(ServeTlsError::Unusable { .. })),
        "a certificate that does not belong to the key must refuse the boot"
    );
}

/// THE REFUSAL ABOVE HAS TO SAY WHAT WAS WRONG, and the case above cannot tell.
///
/// `matches!` on the variant passes whether `detail` was built by walking the
/// error's `source()` chain or by a bare `e.to_string()` — so on its own it is a
/// certifying fixture, green under both. That is the exact defect telemetry#12
/// corrected in the shared unit `boot::server` now calls.
///
/// **TWO LAYERS AND ONE `source()` HOP, read out of the dependencies rather than
/// inferred from rendered output.** The head is `tonic::transport::Error`: its
/// `Display` is `f.write_str(self.description())` (tonic 0.14.6
/// `src/transport/error.rs:79-83`), and `description()` returns the literal
/// `"transport error"` for `Kind::Transport` (`:52-54`) without ever consulting
/// the source. One hop down is `rustls::Error`, which is a LEAF — `impl
/// std::error::Error for Error {}`, the default `source()` returning `None`
/// (rustls 0.23.43 `src/error.rs:1022`) — and whose `InconsistentKeys` arm
/// renders `keys may not be consistent: {why:?}` (`:1003-1005`).
///
/// **`KeyMismatch` IS NOT A THIRD LAYER, and an earlier version of this comment
/// said it was.** It is the `{why:?}` INSIDE rustls's single `Display` string —
/// the `Debug` of a `#[non_exhaustive]`, `Copy` enum (`:121-133`). The colon in
/// front of it is rustls's own punctuation, not a join this walk performed.
/// Reading a layer boundary out of a colon in rendered output is exactly the
/// mistake ADR-0591 exists to stop, and the correction came from reading the two
/// crates rather than from anything CI reported.
///
/// So the assertion below is on the DELIBERATE `Display` string and NOT on that
/// `Debug`: a `#[non_exhaustive]` enum's `Debug` is the least stable text in the
/// chain, and pinning it would redden five repositories at once for a change
/// that is not a defect. Both spellings discriminate identically — neither
/// appears anywhere in tonic's two words — so nothing is given up by choosing
/// the stable one.
///
/// A `detail` carrying `keys may not be consistent` therefore crossed the one
/// hop and cannot have come from the head alone. Reverting the call site to
/// `e.to_string()` leaves `detail` as exactly `transport error` and turns this
/// red.
#[tokio::test]
async fn the_refusal_names_the_reason_rather_than_just_transport_error() {
    let served = pki(SERVED_NAME);
    let other = pki(SERVED_NAME);
    let cert = TempPem::with(&served.cert_pem);
    let key = TempPem::with(&other.key_pem);

    let tls = serve_tls(cert.path(), key.path());
    let Err(error) = boot::server(Some(&tls)) else {
        panic!("a certificate that does not belong to the key must refuse the boot");
    };
    // `boot::refusal` is the flattening `main` prints: lifecycle keeps tonic's
    // error as the SOURCE of `Unusable` rather than in its `Display`, so a bare
    // `to_string()` would stop at "is unusable" and name no reason.
    let detail = boot::refusal(&error);

    assert!(
        detail.contains("keys may not be consistent"),
        "the detail must carry the layer UNDER tonic's `transport error`, which is \
         the only part naming what was wrong"
    );
    assert_ne!(
        detail.trim(),
        "transport error",
        "the head of the chain alone says nothing an operator can act on"
    );
}
