//! Controller HTTP(S) service wiring.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lu_config::Config;
use lu_controller::{build_router, recover_on_boot, run_background, AppState as ControllerState};
use lu_serve::{serve, shutdown_signal, ServeOptions};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Value checks shared by `lu check` and `lu serve`: parsing, token and role rules, schedule
/// and limits sections, paths, and compiled scrub rules. No port is bound, no database is opened
/// and no secret file is read here. `lu serve` calls [`lu_config::check_secrets`] after this.
pub fn validate(path: &Path) -> Result<Config, String> {
    let config = lu_config::load_values(path).map_err(|error| error.to_string())?;
    lu_controller::Scrubber::validate_config(config.artifacts.as_ref())
        .map_err(|error| format!("invalid [artifacts] config: {error}"))?;
    Ok(config)
}

/// Binds the listener and serves until a shutdown signal.
pub async fn run(config: &Config, config_path: &Path) -> Result<(), String> {
    let controller = config
        .controller
        .as_ref()
        .ok_or_else(|| "serve requires a [controller] config".to_owned())?;

    let store = lu_store::sqlite::SqliteStore::open(&controller.db)
        .map_err(|e| format!("cannot open {}: {e:?}", controller.db))?;
    let store: Arc<dyn lu_store::Store> = Arc::new(store);
    let mut state = ControllerState::from_config_with_all(
        controller,
        store,
        config.artifacts.as_ref(),
        config.scope.as_ref(),
        config.drafts.as_ref(),
        config.intake.clone(),
        config.mirror.as_ref(),
    )?;
    state.set_dispatch_limits(lu_store::DispatchLimits {
        min_disk_free_pct: config.min_disk_free_pct(),
    });
    let state = Arc::new(state);
    if let Some(publish) = config.publish.clone() {
        state.set_publish(publish);
    }
    state.set_config_path(config_path);
    state.set_schedule(config.schedule().cloned());
    let audit_task = match &config.audit {
        Some(audit) => {
            let (writer, task) = lu_audit::Writer::open(
                Path::new(&audit.path),
                Arc::clone(&state.audit_write_errors),
            )
            .await
            .map_err(|e| format!("cannot open audit file {}: {e}", audit.path))?;
            state.set_audit(writer);
            tracing::info!(path = %audit.path, "audit record enabled");
            Some(task)
        }
        None => None,
    };
    // A recovery failure must stop startup: otherwise the process would serve stale
    // leases that appeared unexpired against a fresh monotonic clock.
    recover_on_boot(&state).await?;
    {
        let tls = match &config.tls {
            Some(tls) => Some(lu_serve::tls::load(
                Path::new(&tls.cert),
                Path::new(&tls.key),
            )?),
            None => None,
        };
        let listener = TcpListener::bind(&controller.listen)
            .await
            .map_err(|e| format!("cannot listen on {}: {e}", controller.listen))?;
        let addr: SocketAddr = listener.local_addr().map_err(|e| e.to_string())?;
        tracing::info!(%addr, tls = tls.is_some(), config = %config_path.display(), "listening");
        let options = ServeOptions {
            max_connections: controller.max_connections,
            max_connections_per_addr: controller.max_connections_per_addr,
            header_read_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            connection_idle_timeout: Duration::from_secs(120),
        };

        let (tx, _) = broadcast::channel::<()>(1);
        let mut rx_serve = tx.subscribe();
        let mut rx_bg = tx.subscribe();
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = tx.send(());
        });
        let bg = tokio::spawn(run_background(Arc::clone(&state), async move {
            let _ = rx_bg.recv().await;
        }));
        serve(
            listener,
            build_router(Arc::clone(&state)),
            options,
            tls,
            async move {
                let _ = rx_serve.recv().await;
            },
        )
        .await;
        let _ = bg.await;
        drop(state);
        if let Some(task) = audit_task {
            let _ = task.await;
        }
        tracing::info!("stopped");
        Ok(())
    }
}
