use anyhow::Result;
use sessiontap_adapters::AdapterRegistry;
use sessiontap_core::{
    config::{Config, SinkConfig},
    paths::AppPaths,
};
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
    control::{ControlChannel, RECONNECT_BACKOFF, control_url},
    server::handle,
    sinks::{TokenSource, build_sinks},
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
    let worker = SinkWorker::new(&app, sinks, &daemon);
    worker.reset_baselines()?;
    tokio::spawn(worker.run(daemon.sink_poll()));
    tokio::spawn(stale_working_worker(app.clone(), daemon.stale_sweep()));
    for (name, sink) in &config.sinks {
        let SinkConfig::Hub {
            url,
            token_env,
            token_file,
            ..
        } = sink
        else {
            continue;
        };
        if !sink.controls_terminals() {
            continue;
        }
        let config_file = paths.config_file();
        let sink_name = name.clone();
        // re-read at every open and input so turning control off applies
        // without a restart
        let enabled = Arc::new(move || {
            load_config(&config_file)
                .ok()
                .and_then(|config| {
                    config
                        .sinks
                        .get(&sink_name)
                        .map(SinkConfig::controls_terminals)
                })
                .unwrap_or(false)
        });
        let channel = ControlChannel {
            sink: name.clone(),
            source_id: config.source_id.clone().unwrap_or_default(),
            url: control_url(url)?,
            auth: TokenSource::from_config(token_env.as_deref(), token_file.as_deref()),
            enabled,
            backoff: RECONNECT_BACKOFF,
        };
        tokio::spawn(channel.run(app.clone()));
    }
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
