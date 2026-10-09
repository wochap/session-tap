//! End-to-end terminal relay: an in-process hub, a real `sessiontapd` with
//! a control-enabled hub sink, and an agent in an isolated tmux server,
//! driven by paired devices over the remote protocol. Runs when `tmux` is
//! on `PATH`.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sessiontap_hub::{
    config::RemoteConfig,
    ingest::{self, IngestAuth},
    remote::{CLOSE_REVOKED, RemoteGate, RemoteLimits, serve_remote},
    scope::Scope,
    service::{self, Hub, RemoteInfo},
    store::HubStore,
    tls::{Identity, client_config, server_config},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::broadcast, time::timeout};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Phone = WebSocketStream<TlsStream<tokio::net::TcpStream>>;

const PROVIDER: &str = r#"#!/bin/sh
echo "provider-ready:$$"
while IFS= read -r line; do
  echo "got:$line"
  [ "$line" = quit ] && exit 0
done
"#;

struct Env {
    dir: tempfile::TempDir,
    wrapper: PathBuf,
    socket: String,
    daemon: Option<Child>,
}

impl Env {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let root = self.dir.path();
        let mut command = Command::new(program);
        command
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = self
            .command("tmux")
            .args(["-S", &self.socket])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "tmux {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", &self.socket, "kill-server"])
            .status();
        if let Some(daemon) = &mut self.daemon {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

async fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn start_hub() -> (Arc<Hub>, String, std::net::SocketAddr, std::net::SocketAddr) {
    let identity = Identity::generate("e2e-hub").unwrap();
    let hub_id = identity.spki_sha256();
    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote.local_addr().unwrap();
    let (updates, _) = broadcast::channel(64);
    let hub = Arc::new(Hub::new(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        Some(RemoteInfo {
            hub_id: hub_id.clone(),
            hub_name: "E2E".into(),
            hub_spki: identity.spki.clone(),
            remote: RemoteConfig {
                name: None,
                listen: vec![remote_addr.to_string()],
                advertise: Vec::new(),
                control: true,
                discovery: false,
            },
            interfaces: Vec::new,
        }),
    ));
    let limits = RemoteLimits::default();
    tokio::spawn(serve_remote(
        remote,
        tokio_rustls::TlsAcceptor::from(server_config(&identity).unwrap()),
        Arc::clone(&hub),
        RemoteGate::new(&limits),
        limits,
    ));
    let ingest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ingest_addr = ingest_listener.local_addr().unwrap();
    {
        let hub = Arc::clone(&hub);
        let auth = Arc::new(IngestAuth::new(&BTreeMap::new()));
        tokio::spawn(async move {
            loop {
                let (stream, _) = ingest_listener.accept().await.unwrap();
                let hub = Arc::clone(&hub);
                let auth = Arc::clone(&auth);
                tokio::spawn(async move {
                    let store = Arc::clone(&hub.store);
                    let relay = Arc::clone(&hub.relay);
                    if let Some(publication) =
                        ingest::serve_connection(stream, store, auth, 1024 * 1024, relay).await
                    {
                        let _ = hub.updates.send(publication);
                    }
                });
            }
        });
    }
    (hub, hub_id, remote_addr, ingest_addr)
}

fn pair(hub: &Hub) -> Identity {
    let identity = Identity::generate("phone").unwrap();
    hub.pair_device(
        &identity.spki_sha256(),
        "Phone",
        &Scope::expand(&[Scope::Control]),
    )
    .unwrap();
    identity
}

async fn phone(hub_id: &str, addr: std::net::SocketAddr, identity: &Identity) -> Phone {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let tls = TlsConnector::from(client_config(hub_id, Some(identity)).unwrap())
        .connect(ServerName::try_from("hub").unwrap(), tcp)
        .await
        .unwrap();
    tokio_tungstenite::client_async("wss://hub/", tls)
        .await
        .unwrap()
        .0
}

async fn recv(ws: &mut Phone) -> Option<Value> {
    loop {
        match timeout(Duration::from_secs(20), ws.next()).await.ok()?? {
            Ok(Message::Text(text)) => return serde_json::from_str(text.as_str()).ok(),
            Ok(Message::Close(frame)) => {
                return Some(json!({"closed": frame.map(|frame| u16::from(frame.code))}));
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// A device's view of one terminal stream: pane text and the end reason.
struct Watch {
    ws: Phone,
    text: String,
    ended: Option<String>,
    next_id: u64,
}

impl Watch {
    /// Sends a request and returns its response, collecting pushed frames.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.ws
            .send(Message::text(
                json!({"id": id, "method": method, "params": params}).to_string(),
            ))
            .await
            .unwrap();
        loop {
            let message = recv(&mut self.ws).await.expect("response");
            if message["id"] == id {
                return message;
            }
            self.absorb(&message);
        }
    }

    fn absorb(&mut self, message: &Value) {
        if message["type"] != "terminal" {
            return;
        }
        let frame = &message["frame"];
        match frame["type"].as_str() {
            Some("snapshot" | "output") => {
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(frame["data"].as_str().unwrap())
                    .unwrap();
                self.text.push_str(&String::from_utf8_lossy(&data));
            }
            Some("ended") => self.ended = frame["reason"].as_str().map(str::to_owned),
            _ => {}
        }
    }

    async fn wait(&mut self, done: impl Fn(&Self) -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < deadline, "{what}: {:?}", self.text);
            let message = recv(&mut self.ws).await.expect("frame");
            self.absorb(&message);
        }
    }

    async fn open(
        hub_id: &str,
        addr: std::net::SocketAddr,
        identity: &Identity,
        id: &str,
    ) -> (Self, u64) {
        let mut watch = Self {
            ws: phone(hub_id, addr, identity).await,
            text: String::new(),
            ended: None,
            next_id: 0,
        };
        let opened = watch
            .call(
                "terminal.open",
                json!({"source_id": "host", "invocation_id": id}),
            )
            .await;
        let stream = opened["result"]["stream"]
            .as_u64()
            .unwrap_or_else(|| panic!("{opened}"));
        (watch, stream)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_opens_drives_revokes_and_ends() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping relay end-to-end test: tmux is not on PATH");
        return;
    }
    let daemon_bin = Path::new(env!("CARGO_BIN_EXE_sessiontapd"));
    let wrapper = daemon_bin.with_file_name("sessiontap");
    if !wrapper.exists() {
        eprintln!(
            "skipping relay end-to-end test: build the sessiontap binary first (cargo test --workspace)"
        );
        return;
    }
    let (hub, hub_id, remote, ingest) = start_hub().await;
    let dir = tempfile::Builder::new()
        .prefix("str")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("t").to_string_lossy().into_owned();
    let mut env = Env {
        dir,
        wrapper,
        socket,
        daemon: None,
    };
    let root = env.dir.path().to_owned();
    let provider = root.join("fake-provider");
    fs::write(&provider, PROVIDER).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(root.join("config/sessiontap")).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    fs::write(
        root.join("config/sessiontap/config.toml"),
        format!(
            "version = 1\nsource_id = \"host\"\n\n[adapters.fake]\nexecutable = {:?}\ninherits = \"claude\"\n\n[sinks.hub]\ntype = \"hub\"\nenabled = true\nurl = \"http://{ingest}/ingest\"\ncontrol = true\n",
            provider.to_string_lossy()
        ),
    )
    .unwrap();
    env.daemon = Some(
        env.command(daemon_bin)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("control channel", || {
        hub.relay.has_channel("host").then_some(())
    })
    .await;

    env.tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-x",
        "120",
        "-y",
        "30",
        "-s",
        "e2e",
        &format!("'{}' fake", env.wrapper.display()),
    ]);
    let id = wait_until("agent on the hub", || {
        let (_, _, agents) = hub.store.merged().ok()?;
        agents.iter().find_map(|agent| {
            (agent.source_id == "host" && agent.view.terminal.is_some())
                .then(|| agent.view.invocation_id.to_string())
        })
    })
    .await;

    let first = pair(&hub);
    let second = pair(&hub);
    let (mut a, stream_a) = Watch::open(&hub_id, remote, &first, &id).await;
    a.wait(|watch| watch.text.contains("provider-ready:"), "snapshot")
        .await;
    let sent = a
        .call(
            "terminal.input",
            json!({"stream": stream_a, "keys": ["x", "enter"]}),
        )
        .await;
    assert_eq!(sent["result"], json!({}), "{sent}");
    a.wait(|watch| watch.text.contains("got:x"), "input echoed")
        .await;

    let (mut b, stream_b) = Watch::open(&hub_id, remote, &second, &id).await;
    b.wait(|watch| watch.text.contains("got:x"), "second snapshot")
        .await;

    hub.revoke(&service::device_id(&first.spki_sha256()))
        .unwrap();
    loop {
        let message = recv(&mut a.ws).await;
        match message {
            Some(message) if message.get("closed").is_some() => {
                assert_eq!(message["closed"], CLOSE_REVOKED);
                break;
            }
            Some(_) => {}
            None => panic!("revoked device saw no close code"),
        }
    }
    assert_eq!(
        hub.relay.stream_count(),
        1,
        "the revoked stream is released"
    );

    let quit = b
        .call(
            "terminal.input",
            json!({"stream": stream_b, "paste": {"text": "quit", "enter": true}}),
        )
        .await;
    assert_eq!(quit["result"], json!({}), "{quit}");
    b.wait(|watch| watch.ended.is_some(), "agent exit").await;
    assert_eq!(b.ended.as_deref(), Some("agent_exited"));
    let after = b
        .call("terminal.input", json!({"stream": stream_b, "keys": ["1"]}))
        .await;
    assert_eq!(after["error"]["code"], "terminal_ended", "{after}");
}
