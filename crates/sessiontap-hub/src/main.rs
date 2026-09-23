use anyhow::{Context, Result, bail};
use sessiontap_hub::config::{HubConfig, Subscription};
use sessiontap_hub::ingest::{self, HubPublication};
use sessiontap_hub::listen::{self, HubRequest};
use sessiontap_hub::paths::HubPaths;
use sessiontap_hub::routing::CommandLimits;
use sessiontap_hub::store::HubStore;
use sessiontap_infra::{
    fs::prepare_private_dir,
    json::write_json_line,
    socket::{bind_error, bind_private_unix_socket},
};
use std::{fs, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, UnixStream},
    sync::broadcast,
};

const BROADCAST_CAPACITY: usize = 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("run") => run_service().await,
        Some("listen") => listen_client().await,
        Some("--help" | "-h") => {
            eprintln!("usage: sessiontap-hub [run] | listen");
            Ok(())
        }
        Some(other) => bail!("usage: sessiontap-hub [run] | listen (unknown command '{other}')"),
    }
}

async fn run_service() -> Result<()> {
    let paths = HubPaths::discover()?;
    prepare_private_dir(&paths.runtime_dir)?;
    prepare_private_dir(&paths.state_dir)?;
    let socket = paths.socket();
    let (unix_listener, lock) = bind_private_unix_socket(&socket, &paths.lock())
        .map_err(|error| bind_error("sessiontap-hub", &socket, error))?;
    let config = HubConfig::load(&paths.config_file()).unwrap_or_else(|e| {
        eprintln!("sessiontap-hub: configuration disabled: {e}");
        HubConfig::default()
    });
    let store = Arc::new(HubStore::open(&paths.database())?);
    store.prune_retained(config.retention_days)?;
    let (updates, _) = broadcast::channel::<HubPublication>(BROADCAST_CAPACITY);
    tokio::spawn(retention_task(Arc::clone(&store), config.retention_days));
    for subscription in &config.subscriptions {
        let label = subscription
            .name
            .clone()
            .unwrap_or_else(|| "unnamed".into());
        eprintln!("sessiontap-hub: subscription '{label}' active");
    }
    let subscriptions = Arc::new(config.subscriptions.clone());
    tokio::spawn(route_updates(
        updates.subscribe(),
        subscriptions,
        CommandLimits::from_config(&config),
    ));
    let tcp_listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("bind ingestion address {}", config.listen))?;
    eprintln!(
        "sessiontap-hub: ingesting on {} and serving merged stream on {}",
        config.listen,
        socket.display()
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = tcp_listener.accept() => {
                let (stream, _) = accepted?;
                let store = Arc::clone(&store);
                let token_file = config.token_file.clone();
                let max_body = config.max_body_bytes;
                let sender = updates.clone();
                tokio::spawn(async move {
                    if let Some(publication) =
                        ingest::serve_connection(stream, store, token_file, max_body).await
                    {
                        let _ = sender.send(publication);
                    }
                });
            }
            accepted = unix_listener.accept() => {
                let (stream, _) = accepted?;
                let store = Arc::clone(&store);
                let receiver = updates.subscribe();
                tokio::spawn(async move {
                    let _ = listen::serve_listener(stream, store, receiver).await;
                });
            }
        }
    }
    let _ = fs::remove_file(&socket);
    drop(lock);
    Ok(())
}

async fn retention_task(store: Arc<HubStore>, retention_days: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Err(error) = store.prune_retained(retention_days) {
            eprintln!("sessiontap-hub: retention pruning failed: {error}");
        }
    }
}

/// Evaluates subscriptions only for durably accepted updates. Rejected, stale,
/// and duplicate deliveries never reach this task; source snapshots carry no
/// routable update.
async fn route_updates(
    mut receiver: broadcast::Receiver<HubPublication>,
    subscriptions: Arc<Vec<Subscription>>,
    limits: CommandLimits,
) {
    loop {
        match receiver.recv().await {
            Ok(HubPublication::Update(update)) => {
                if !subscriptions.is_empty() {
                    sessiontap_hub::routing::dispatch(
                        Arc::clone(&subscriptions),
                        *update,
                        limits.clone(),
                    );
                }
            }
            Ok(HubPublication::SnapshotApplied { .. }) => {}
            Err(broadcast::error::RecvError::Lagged(count)) => {
                eprintln!("sessiontap-hub: routing lagged by {count} updates");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Client for the merged stream: one baseline snapshot then JSONL updates.
async fn listen_client() -> Result<()> {
    let paths = HubPaths::discover()?;
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("connect sessiontap-hub; is the service running?")?;
    write_json_line(&mut stream, &HubRequest::Listen).await?;
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
    }
    Ok(())
}
