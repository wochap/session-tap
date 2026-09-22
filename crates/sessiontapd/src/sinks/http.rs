use super::{DeliveryOutcome, Sink, TokenSource, post, project_fields};
use async_trait::async_trait;
use sessiontap_core::domain::PublicField;
use std::{collections::BTreeSet, time::Duration};

/// Plain HTTP receiver. Any 2xx acknowledges; every other status, timeout,
/// transport error, or token read failure is retried.
pub struct HttpSink {
    pub(super) name: String,
    pub(super) url: String,
    pub(super) auth: TokenSource,
    pub(super) timeout: Duration,
    pub(super) fields: BTreeSet<PublicField>,
    pub(super) client: reqwest::Client,
}

#[async_trait]
impl Sink for HttpSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn deliver(&self, payload: &[u8]) -> DeliveryOutcome {
        let Some(body) = project_fields(payload, &self.fields) else {
            return DeliveryOutcome::Reject;
        };
        match post(&self.client, &self.url, &self.auth, self.timeout, body).await {
            Ok(response) if response.status().is_success() => DeliveryOutcome::Ack,
            Ok(_) => DeliveryOutcome::Retry,
            Err(error) => {
                eprintln!(
                    "sessiontapd: http sink '{}' delivery failed: {error}",
                    self.name
                );
                DeliveryOutcome::Retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sinks::tests::{responder, update_payload};

    fn sink(url: String, fields: BTreeSet<PublicField>, timeout: Duration) -> HttpSink {
        HttpSink {
            name: "archive".into(),
            url,
            auth: TokenSource::None,
            timeout,
            fields,
            client: reqwest::Client::new(),
        }
    }

    const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    #[tokio::test]
    async fn empty_selection_delivers_the_full_view() {
        let (url, task) = responder(vec![NO_CONTENT]).await;
        let payload = update_payload();
        let outcome = sink(url, BTreeSet::new(), Duration::from_secs(2))
            .deliver(&payload)
            .await;
        assert_eq!(outcome, DeliveryOutcome::Ack);
        assert_eq!(task.await.unwrap(), vec![payload]);
    }

    #[tokio::test]
    async fn configured_selection_delivers_only_those_fields() {
        let (url, task) = responder(vec![NO_CONTENT]).await;
        let fields = BTreeSet::from([PublicField::Status, PublicField::Usage]);
        let outcome = sink(url, fields, Duration::from_secs(2))
            .deliver(&update_payload())
            .await;
        assert_eq!(outcome, DeliveryOutcome::Ack);
        let body: serde_json::Value = serde_json::from_slice(&task.await.unwrap()[0]).unwrap();
        let mut keys: Vec<_> = body["view"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["invocation_id", "status", "usage"]);
        assert_eq!(body["changed"], serde_json::json!(["status", "usage"]));
    }

    #[tokio::test]
    async fn silent_listener_past_timeout_is_retried() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/events", listener.local_addr().unwrap());
        let hold = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
            drop(stream);
        });
        let started = std::time::Instant::now();
        let outcome = sink(url, BTreeSet::new(), Duration::from_millis(500))
            .deliver(&update_payload())
            .await;
        assert_eq!(outcome, DeliveryOutcome::Retry);
        assert!(started.elapsed() < Duration::from_secs(2));
        hold.abort();
    }

    #[tokio::test]
    async fn server_error_is_retried() {
        let (url, task) = responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        ])
        .await;
        let outcome = sink(url, BTreeSet::new(), Duration::from_secs(2))
            .deliver(&update_payload())
            .await;
        assert_eq!(outcome, DeliveryOutcome::Retry);
        task.await.unwrap();
    }
}
