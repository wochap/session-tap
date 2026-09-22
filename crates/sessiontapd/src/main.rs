use anyhow::Result;
use sessiontap_adapters::AdapterRegistry;
use sessiontap_core::{config::Config, multiplexer::TmuxAdapter, paths::AppPaths};
use sessiontap_storage::Storage;
use sessiontapd::{
    app::{App, Collection, PublishConfig},
    server::{acquire_daemon_lock, bind_private_socket, handle, process_alive},
    sinks::build_sinks,
    workers::{SinkWorker, stale_working_worker},
};
use std::{fs, path::Path, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    let paths = AppPaths::discover()?;
    AppPaths::prepare_private(&paths.runtime_dir)?;
    AppPaths::prepare_private(&paths.state_dir)?;
    let lock = acquire_daemon_lock(&paths.lock())?;
    let socket = paths.socket();
    let listener = bind_private_socket(&socket).await?;
    let config = Config::load(&paths.config_file()).unwrap_or_else(|e| {
        eprintln!("sessiontapd: configuration disabled: {e}");
        Config::default()
    });
    if let Err(e) = config.validate() {
        anyhow::bail!("sessiontapd: invalid configuration: {e}");
    }
    let sinks = build_sinks(&config.sinks)?;
    let daemon = config.daemon.clone();
    let app = App::new(
        Arc::new(Storage::open(&paths.database())?),
        PublishConfig {
            sinks: config.sinks.clone(),
            source_id: config.source_id.clone().unwrap_or_default(),
            source_name: config.source_name.clone(),
        },
        &daemon,
        Arc::new(TmuxAdapter),
        Collection {
            home: std::env::var_os("HOME").map_or_else(|| Path::new("/").to_path_buf(), Into::into),
            registry: Arc::new(AdapterRegistry::new(&config)),
        },
    );
    app.reconcile(process_alive, config.retention_days)?;
    tokio::spawn(SinkWorker::new(&app, sinks, &daemon).run(daemon.sink_poll()));
    tokio::spawn(stale_working_worker(app.clone(), daemon.stale_sweep()));
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let app = app.clone();
                tokio::spawn(async move {
                    let _ = handle(stream, app).await;
                });
            }
        }
    }
    let _ = fs::remove_file(&socket);
    drop(lock);
    Ok(())
}
