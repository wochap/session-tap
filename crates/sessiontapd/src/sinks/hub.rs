use super::{DeliveryOutcome, Sink, TokenSource, post};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

/// Canonical hub receiver. Delivers envelopes unchanged and requires a source
/// snapshot baseline before incremental updates.
pub struct HubSink {
    pub(super) name: String,
    pub(super) url: String,
    pub(super) auth: TokenSource,
    pub(super) timeout: Duration,
    pub(super) client: reqwest::Client,
}

/// Hub error body, for example `{"error":"snapshot_required"}`.
#[derive(Deserialize)]
struct HubError {
    error: String,
}

impl HubSink {
    async fn post(&self, payload: &[u8]) -> DeliveryOutcome {
        let response = match post(
            &self.client,
            &self.url,
            &self.auth,
            self.timeout,
            payload.to_vec(),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                eprintln!(
                    "sessiontapd: hub sink '{}' delivery failed: {error}",
                    self.name
                );
                return DeliveryOutcome::Retry;
            }
        };
        let status = response.status();
        if status.is_success() {
            return DeliveryOutcome::Ack;
        }
        if status == reqwest::StatusCode::CONFLICT {
            let body = response.bytes().await.unwrap_or_default();
            return match serde_json::from_slice::<HubError>(&body) {
                Ok(error) if error.error == "snapshot_required" => {
                    DeliveryOutcome::SnapshotRequired
                }
                _ => DeliveryOutcome::Reject,
            };
        }
        if status.is_client_error() {
            return DeliveryOutcome::Reject;
        }
        DeliveryOutcome::Retry
    }
}

#[async_trait]
impl Sink for HubSink {
    fn name(&self) -> &str {
        &self.name
    }

    fn needs_baseline(&self) -> bool {
        true
    }

    async fn deliver(&self, payload: &[u8]) -> DeliveryOutcome {
        self.post(payload).await
    }

    async fn deliver_snapshot(&self, payload: &[u8]) -> DeliveryOutcome {
        self.post(payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sinks::tests::{responder, update_payload};
    use std::{fs, os::unix::fs::PermissionsExt};

    fn sink(url: String, auth: TokenSource) -> HubSink {
        HubSink {
            name: "hub".into(),
            url,
            auth,
            timeout: Duration::from_secs(2),
            client: reqwest::Client::new(),
        }
    }

    async fn outcome_for(response: &'static str) -> DeliveryOutcome {
        let (url, task) = responder(vec![response]).await;
        let payload = update_payload();
        let outcome = sink(url, TokenSource::None).deliver(&payload).await;
        assert_eq!(
            task.await.unwrap(),
            vec![payload],
            "hub bytes are unchanged"
        );
        outcome
    }

    fn response(status: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[tokio::test]
    async fn success_acknowledges() {
        assert_eq!(
            outcome_for(response("200 OK", "{}")).await,
            DeliveryOutcome::Ack
        );
    }

    #[tokio::test]
    async fn structured_snapshot_required_conflict() {
        assert_eq!(
            outcome_for(response("409 Conflict", r#"{"error":"snapshot_required"}"#)).await,
            DeliveryOutcome::SnapshotRequired
        );
    }

    #[tokio::test]
    async fn other_conflict_codes_and_unparsable_bodies_are_rejected() {
        assert_eq!(
            outcome_for(response("409 Conflict", r#"{"error":"stale_revision"}"#)).await,
            DeliveryOutcome::Reject
        );
        assert_eq!(
            outcome_for(response("409 Conflict", "snapshot_required, please")).await,
            DeliveryOutcome::Reject
        );
    }

    #[tokio::test]
    async fn client_errors_reject_and_server_errors_retry() {
        assert_eq!(
            outcome_for(response(
                "400 Bad Request",
                r#"{"error":"malformed_envelope"}"#
            ))
            .await,
            DeliveryOutcome::Reject
        );
        assert_eq!(
            outcome_for(response("503 Service Unavailable", "")).await,
            DeliveryOutcome::Retry
        );
    }

    #[tokio::test]
    async fn transport_error_retries() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/ingest", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            sink(url, TokenSource::None).deliver(b"{}").await,
            DeliveryOutcome::Retry
        );
    }

    #[tokio::test]
    async fn token_file_error_retries_without_sending() {
        let temp = tempfile::tempdir().unwrap();
        let token = temp.path().join("token");
        fs::write(&token, "secret").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        let hub = sink("http://127.0.0.1:9/ingest".into(), TokenSource::File(token));
        assert_eq!(hub.deliver(b"{}").await, DeliveryOutcome::Retry);
        assert_eq!(hub.deliver_snapshot(b"{}").await, DeliveryOutcome::Retry);
    }
}
