//! Boot order is the decision here, not the wiring.
//!
//! Probe, migrate, then serve — and the service does not report ready until all
//! three have succeeded. D7 makes a capability gap a boot failure, and D69 puts
//! the probe before the pool is declared ready precisely so a failure is a
//! crash-loop rather than a pod that accepts traffic and fails queries. Under
//! D68 the second shape is actively harmful: a pod that starts and then errors is
//! one the HPA adds replicas around.

use std::net::SocketAddr;

use sqlx::Connection;
use yadgar_store::capability::{Capability, CapabilitySet};
use yadgar_store::credentials::{CredentialSource, Secret};
use yadgar_store::{migrate, probe};

use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbServiceServer;
use yadgar_task_db::{boot, schema, service::TaskDb};

/// What this module needs of its engine (D69). Addressed, not ranked — so no
/// vector search and no full-text (D10). Requiring either would make this module
/// refuse to boot on an engine that serves it perfectly well.
fn required() -> CapabilitySet {
    CapabilitySet::from([Capability::Transactions, Capability::RowLocking])
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        // A DEFAULT, because from_default_env() with RUST_LOG unset enables
        // NOTHING — the service runs silently and its boot sequence, its
        // capability probe result and its errors all vanish. Found by deploying:
        // two replicas were Running and `kubectl logs` returned nothing at all,
        // so the only way to see why one had restarted was the previous
        // container's exit output.
        //
        // A service nobody can observe is one D67 cannot measure either.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Every default, every refusal and both transport modes live in `boot`,
    // which a test can reach. These lines are the whole of the configuration
    // decision.
    //
    // `.to_string()` on the way out, and not decoration: `main` returns
    // `Box<dyn Error>`, which Rust prints with DEBUG — so a bare `?` put
    // `ObsoleteRequireTls` on the operator's terminal instead of the paragraph
    // saying which key to set instead. Every refusal in `boot` is written as a
    // sentence for somebody reading a crash loop, and Debug threw all of them
    // away. `task` has stringified for the same reason since its own transport
    // landed.
    let config = boot::pool_config(|key| std::env::var(key).ok()).map_err(|e| e.to_string())?;

    // THE LISTENER'S transport, read and CHECKED before anything else — the PEM
    // decoded, the certificate matched against its key. A deployment that asked
    // for TLS and got the mount wrong exits here, without opening a socket and
    // without touching the engine. D69 puts the refusals first, and this one is
    // cheaper than the probe.
    //
    // `boot::server` is the only server construction in this binary, which is
    // structural rather than tidy: the downgrade this guards against is a
    // listener that opens in cleartext because TLS configuration failed, and
    // with one construction site there is nowhere else to write it.
    let tls = boot::ServeTls::from_env(boot::LISTEN).map_err(|e| e.to_string())?;
    let mut server = boot::server(tls.as_ref()).map_err(|e| e.to_string())?;

    // The credential never arrives as an environment variable — it is a mounted
    // Secret the operator issued (D58), read through the seam so this module has
    // no idea which deployment target it is on.
    let secret: Secret = CredentialSource::SecretFile(
        env_or("DB_PASSWORD_FILE", "/var/run/secrets/task-db/password").into(),
    )
    .resolve()?;

    // 1. PROBE, on a connection of its own and before the pool exists. Refusing
    //    here is the whole point of D7.
    //
    //    Its own CONNECTION, never its own connection OPTIONS. This used to be a
    //    `format!`-ed DSN with no `ssl-mode` in it, so the probe ran on sqlx's
    //    `Preferred` default — which falls back to an unencrypted connection —
    //    while the pool three lines down was on `Required`. The options come
    //    from the same function the pool uses, so the two cannot disagree.
    let mut conn =
        sqlx::MySqlConnection::connect_with(&boot::probe_connect_options(&config, &secret)).await?;
    let report = probe::run(&mut conn).await?;
    report.satisfies(&required())?;
    conn.close().await?;
    tracing::info!("engine satisfies the required capabilities");

    // 2. MIGRATE. Refuses outright if the database is ahead of this binary.
    let pool = yadgar_store::pool::connect(&config, &secret).await?;
    let applied = migrate::apply(&pool, &schema::migrations()?).await?;
    tracing::info!(applied, "schema at migration {applied}");

    // 3. SERVE. Only now.
    // The BINARY installs the exporter, never the library — a library that
    // installs one picks the backend for every service linking it. A failure here
    // is logged and ignored: a service that cannot export metrics should still
    // serve traffic, which is D25's rule applied to the metrics path too.
    let metrics_addr: SocketAddr = env_or("METRICS_LISTEN", "0.0.0.0:9090").parse()?;
    if let Err(e) = yadgar_telemetry::metrics::install_prometheus(metrics_addr) {
        tracing::warn!(error = %e, "metrics endpoint unavailable; continuing without it");
    }

    let addr: SocketAddr = env_or("LISTEN", "0.0.0.0:50051").parse()?;

    // ARMED BEFORE THE SERVER IS SPAWNED, and that ordering is the fix rather
    // than an accident of where the line sits. `boot::shutdown` installs both
    // signal handlers when it is CALLED — a SIGTERM arriving between here and
    // the first poll of the future would otherwise take the process's default
    // disposition and kill it outright, mid-transaction.
    //
    // Stringified like every other refusal in this function, for the reason
    // given above.
    let shutdown = boot::shutdown().map_err(|e| e.to_string())?;

    tracing::info!(%addr, tls = tls.is_some(), "task-db listening");
    server
        .add_service(TaskDbServiceServer::new(TaskDb::new(pool)))
        .serve_with_shutdown(addr, shutdown)
        .await?;

    Ok(())
}
