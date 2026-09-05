//! Boot order is the decision here, not the wiring.
//!
//! Probe, migrate, then serve — and the service does not report ready until all
//! three have succeeded. D7 makes a capability gap a boot failure, and D69 puts
//! the probe before the pool is declared ready precisely so a failure is a
//! crash-loop rather than a pod that accepts traffic and fails queries. Under
//! D68 the second shape is actively harmful: a pod that starts and then errors is
//! one the HPA adds replicas around.

use std::net::SocketAddr;
use std::path::PathBuf;

use sqlx::Connection;
use yadgar_lifecycle::{drain_within, Drain, DRAIN_BUDGET};
use yadgar_store::capability::{Capability, CapabilitySet};
use yadgar_store::credentials::{CredentialSource, Secret};
use yadgar_store::{migrate, probe};

use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbServiceServer;
use yadgar_task_db::{boot, rotate, schema, service::TaskDb};

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
    // THE PATH IS HOISTED INTO A VARIABLE because two things need it: the read
    // below, and the rotation watch set. `Secret` deliberately holds the VALUE
    // and not where it came from, so the path has to be named once here rather
    // than recovered from the secret afterwards.
    let db_password_file: PathBuf =
        env_or("DB_PASSWORD_FILE", "/var/run/secrets/task-db/password").into();
    let secret: Secret = CredentialSource::SecretFile(db_password_file.clone()).resolve()?;

    // THE WATCH SET, ASSEMBLED FROM THE RESOLVED CONFIGURATION AND HASHED AS THE
    // PROCESS READS IT (ADR-0523). It is built HERE, immediately after the last
    // of its members is read, rather than at the point the watcher is spawned:
    // deferring the first reading to the watcher's first poll would put the whole
    // of probe-migrate-serve inside a window where a kubelet swap quietly becomes
    // the baseline, and the real rotation would never be noticed.
    //
    // THREE MATERIALS, TWO OF WHICH ARE NOT THE CERTIFICATE. ADR-0523's rule is
    // about provenance rather than payload — the database password is read once
    // and baked into a pool that outlives every reconnect, and the engine's CA is
    // mounted the same way — so both are watched exactly as the leaf is.
    //
    // ONE CALL, AND THE SAME ONE `tests/assembly.rs` MAKES. Nothing in a binary
    // entry point is reachable from a test, so a member deleted from a list built
    // HERE would compile, pass everything, and ship a process blind to that file.
    // The list lives in `rotate::watch_set`.
    let tls_inputs = rotate::watch_set(tls.as_ref(), &db_password_file, config.ssl_ca.as_deref());

    // How often those files are re-hashed, and how long THIS pod waits before
    // acting on a change. The splay is what stops both replicas exiting inside
    // the same kubelet sync window — a PDB constrains eviction and does not
    // govern a self-exit.
    //
    // PARSED AT BOOT rather than at the first poll, so a mistyped interval fails
    // the boot instead of becoming a hot loop nobody would see.
    let schedule = rotate::Schedule::from_env().map_err(|e| e.to_string())?;

    // 1. PROBE, on a connection of its own and before the pool exists. Refusing
    //    here is the whole point of D7.
    //
    //    Its own CONNECTION, never its own connection OPTIONS. This used to be a
    //    `format!`-ed DSN with no `ssl-mode` in it, so the probe ran on sqlx's
    //    `Preferred` default — which falls back to an unencrypted connection —
    //    while the pool three lines down was on `Required`. The options come
    //    from the same function the pool uses, so the two cannot disagree.
    //
    //    `.to_string()` for the same reason `pool_config` above carries it:
    //    building these options REFUSES `verify_ca`, and that refusal is a
    //    paragraph naming the mode to use instead. A bare `?` would Debug-print
    //    `SslModeCannotVerify { .. }` into the crash loop and throw the sentence
    //    away.
    let options = boot::probe_connect_options(&config, &secret).map_err(|e| e.to_string())?;
    let mut conn = sqlx::MySqlConnection::connect_with(&options).await?;
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

    // AFTER THE EXPORTER, NEVER BEFORE IT: a value recorded while there is no
    // recorder is a value nobody ever sees. This is the half of the rotation work
    // that makes a failure LOUD — if the watcher below dies, this gauge still
    // shows the loaded leaf ageing out.
    tls_inputs.export_not_after();

    let addr: SocketAddr = env_or("LISTEN", "0.0.0.0:50051").parse()?;

    // ARMED BEFORE THE SERVER IS SPAWNED, and that ordering is the fix rather
    // than an accident of where the line sits. `boot::shutdown` is a `fn`
    // returning a future rather than an `async fn`, so both signal handlers
    // install when it is CALLED — a SIGTERM arriving between here and the first
    // poll of the future would otherwise take the process's default disposition
    // and kill it outright, mid-transaction.
    //
    // The behaviour is `yadgar_lifecycle::shutdown`'s; `boot::shutdown` is the
    // three lines that turn its `io::Error` into a `BootError`, so this line
    // reads and fails exactly as it did.
    //
    // Stringified like every other refusal in this function, for the reason
    // given above.
    let signals = boot::shutdown().map_err(|e| e.to_string())?;

    // `watching` is recorded for the reason `tls` is: a zero there is a process
    // that will notice nothing, and it must be answerable from the boot log
    // rather than inferred from which variables somebody believes they set.
    tracing::info!(
        %addr,
        tls = tls.is_some(),
        watching = tls_inputs.watched().len(),
        rotation_poll_secs = schedule.poll().as_secs(),
        rotation_splay_max_secs = schedule.splay_max().as_secs(),
        drain_budget_secs = DRAIN_BUDGET.as_secs(),
        "task-db listening"
    );

    // THE SERVER IS SPAWNED WITH A ONESHOT AS ITS SHUTDOWN FUTURE, and the wait
    // happens OUTSIDE it. `drain_within` starts the budget's clock when shutdown
    // is REQUESTED; a `timeout` wrapped round the serving future itself would fix
    // its deadline at boot and end the process a few seconds later on every boot,
    // having asked nothing to stop.
    let (ask_to_stop, stop_requested) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(
        server
            .add_service(TaskDbServiceServer::new(TaskDb::new(pool)))
            // ONE DRAIN PATH, TWO REASONS TO TAKE IT. `serve_with_shutdown` stops
            // accepting and lets in-flight calls finish, so the rotation exit gets
            // the same drain a signal does rather than a second mechanism beside
            // it.
            .serve_with_shutdown(addr, async {
                let _ = stop_requested.await;
            }),
    );

    // WHAT ENDS THE SERVE, and nothing else does.
    //
    // **THE BUDGET IS PART OF THIS CHANGE RATHER THAN A FOLLOW-UP TO IT.** tokio
    // never unregisters a libc signal handler, so once the rotation arm wins this
    // `select!` a later SIGTERM is SWALLOWED and only SIGKILL remains. A watcher
    // added without `drain_within` would trade an expired certificate for a pod
    // that cannot be stopped politely.
    let stop = async {
        tokio::select! {
            // SIGTERM and SIGINT, already armed above. SIGTERM is the one
            // Kubernetes sends.
            () = signals => {}
            // `rotate::watch` resolves ONLY when it has read a change, and never
            // at all when there is nothing to watch.
            () = rotate::watch(tls_inputs, schedule) => {}
        }
    };

    match drain_within(serving, ask_to_stop, stop, DRAIN_BUDGET).await {
        Drain::Finished(result) => result?,
        // EXIT 0 ANYWAY. The restart is the point; a drain that overran is worth
        // an error in the log, not a CrashLoopBackOff on top of it.
        Drain::Overran => tracing::error!(
            budget_secs = DRAIN_BUDGET.as_secs(),
            "the drain did not finish within its budget; ending anyway with calls still in \
             flight. A request blocked this long is the thing to look at"
        ),
    }

    Ok(())
}
