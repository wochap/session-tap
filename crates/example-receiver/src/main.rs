use anyhow::{Context, Result};
use sessiontap_core::protocol::SinkEvent;
use std::{collections::HashSet, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

#[tokio::main]
async fn main() -> Result<()> {
    let address =
        std::env::var("SESSIONTAP_RECEIVER_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let listener = TcpListener::bind(address).await?;
    let seen = Arc::new(Mutex::new(HashSet::new()));
    loop {
        let (stream, _) = listener.accept().await?;
        let seen = seen.clone();
        tokio::spawn(async move {
            let _ = handle(stream, seen).await;
        });
    }
}
async fn handle(mut stream: TcpStream, seen: Arc<Mutex<HashSet<String>>>) -> Result<()> {
    let mut bytes = Vec::with_capacity(4096);
    let split = loop {
        if bytes.len() > 300_000 {
            anyhow::bail!("request too large");
        }
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            anyhow::bail!("incomplete HTTP request");
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&bytes[..split]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .context("Content-Length required")?;
    while bytes.len() - split < content_length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            anyhow::bail!("incomplete HTTP body");
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let event: SinkEvent = serde_json::from_slice(&bytes[split..split + content_length])?;
    if event.schema_version != 1 {
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }
    let mut seen = seen.lock().await;
    if accept_once(&mut seen, &event.event_id) {
        println!("{}", serde_json::to_string(&event)?);
    }
    stream
        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    Ok(())
}

fn accept_once(seen: &mut HashSet<String>, event_id: &str) -> bool {
    seen.insert(event_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_event_id_prints_once() {
        let mut seen = HashSet::new();
        assert!(accept_once(&mut seen, "event-1"));
        assert!(!accept_once(&mut seen, "event-1"));
    }
}
