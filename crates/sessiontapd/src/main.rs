use anyhow::Result;
use sessiontap_adapters::AdapterRegistry;
use sessiontap_core::{config::Config, paths::AppPaths};
use sessiontap_infra::{
    config::load_config,
    fs::prepare_private_dir,
    multiplexer::MultiplexerRegistry,
    process::process_alive,
    socket::{bind_error, bind_private_unix_socket},
};
use sessiontap_storage::Storage;
use sessiontapd::{
    app::{App, Collection, PublishConfig},
    server::handle,
    sinks::build_sinks,
    workers::{SinkWorker, stale_working_worker},
};
use std::{fs, path::Path, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    let paths = AppPaths::discover()?;
    prepare_private_dir(&paths.runtime_dir)?;
    prepare_private_dir(&paths.state_dir)?;
    let socket = paths.socket();
    let (listener, lock) = bind_private_unix_socket(&socket, &paths.lock())
        .map_err(|error| bind_error("sessiontapd", &socket, error))?;
    let config = load_config(&paths.config_file()).unwrap_or_else(|e| {
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
        Arc::new(MultiplexerRegistry::new()),
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
