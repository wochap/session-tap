use anyhow::{Context, Result, bail};
use sessiontap_core::paths::HubPaths;
use sessiontap_hub::config::{HubConfig, Subscription};
use sessiontap_hub::ingest::{self, HubPublication};
use sessiontap_hub::listen::HubRequest;
use sessiontap_hub::remote::{self, RemoteGate, RemoteLimits};
use sessiontap_hub::routing::CommandLimits;
use sessiontap_hub::scope::Scope;
use sessiontap_hub::service::{self, Hub, RemoteInfo};
use sessiontap_hub::store::HubStore;
use sessiontap_hub::{cli, endpoints, tls};
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
const USAGE: &str = "usage: sessiontap-hub [run]
       sessiontap-hub listen
       sessiontap-hub pair [--scope read|manage|watch|control]...
       sessiontap-hub devices
       sessiontap-hub revoke <device>
       sessiontap-hub forget <source_id> <invocation_id>";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] | ["run"] => run_service().await,
        ["listen"] => listen_client().await,
        ["pair", rest @ ..] => {
            let scopes = parse_scopes(rest)?;
            let socket = HubPaths::discover()?.socket();
            cli::pair(&socket, scopes, &mut std::io::stdout(), true, |_, _| {
                cli::prompt_yes_no("Trust this device?")
            })
            .await
        }
        ["devices"] => cli::devices(&HubPaths::discover()?.socket(), &mut std::io::stdout()).await,
        ["revoke", device] => {
            cli::revoke(
                &HubPaths::discover()?.socket(),
                device,
                &mut std::io::stdout(),
            )
            .await
        }
        ["forget", source_id, invocation_id] => {
            cli::forget(
                &HubPaths::discover()?.socket(),
                source_id,
                invocation_id,
                &mut std::io::stdout(),
            )
            .await
        }
        ["--help" | "-h"] => {
            eprintln!("{USAGE}");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn parse_scopes(args: &[&str]) -> Result<Vec<String>> {
    let mut scopes = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match *arg {
            "--scope" => {
                let Some(scope) = args.next() else {
                    bail!("--scope needs a value ({})", Scope::valid_names());
                };
                scopes.push((*scope).to_owned());
            }
            other => match other.strip_prefix("--scope=") {
                Some(scope) => scopes.push(scope.to_owned()),
                None => bail!("{USAGE}"),
            },
        }
    }
    let scopes = Scope::parse_request(&scopes).map_err(anyhow::Error::msg)?;
    Ok(Scope::names(&scopes))
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
    let auth = Arc::new(ingest::IngestAuth::new(&config.sources));
    let tcp_listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("bind ingestion address {}", config.listen))?;
    let mut remote_info = None;
    let mut remote_listeners = Vec::new();
    if let Some(remote) = &config.remote {
        let hub_name = remote.display_name();
        let identity =
            tls::load_or_create_identity(&paths.state_dir.join(tls::IDENTITY_FILE), &hub_name)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(tls::server_config(&identity)?);
        for address in remote.listen_mode().addresses() {
            remote_listeners.push((address, acceptor.clone()));
        }
        remote_info = Some(RemoteInfo {
            hub_id: identity.spki_sha256(),
            hub_name,
            hub_spki: identity.spki.clone(),
            remote: remote.clone(),
            interfaces: endpoints::system_interfaces,
        });
    }
    let hub = Arc::new(Hub::new(Arc::clone(&store), updates.clone(), remote_info));
    let limits = RemoteLimits::default();
    let gate = RemoteGate::new(&limits);
    for (address, acceptor) in remote_listeners {
        tokio::spawn(remote::supervise_listener(
            address,
            acceptor,
            Arc::clone(&hub),
            Arc::clone(&gate),
            limits.clone(),
            remote::BIND_BACKOFF,
        ));
    }
    eprintln!(
        "sessiontap-hub: ingesting on {} and serving merged stream on {}",
        config.listen,
        socket.display()
    );
    let unix = tokio::spawn(service::serve_unix_listener(
        unix_listener,
        Arc::clone(&hub),
    ));
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
                let auth = Arc::clone(&auth);
                let max_body = config.max_body_bytes;
                let sender = updates.clone();
                let relay = Arc::clone(&hub.relay);
                tokio::spawn(async move {
                    if let Some(publication) =
                        ingest::serve_connection(stream, store, auth, max_body, relay).await
                    {
                        let _ = sender.send(publication);
                    }
                });
            }
        }
    }
    unix.abort();
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

/// Evaluates subscriptions only for durably accepted updates. Rejected,
/// stale, duplicate, and suppressed deliveries never reach this task; source
/// snapshots and forgets carry no routable update.
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
