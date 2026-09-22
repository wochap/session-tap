use super::{DeliveryOutcome, Sink, project_fields};
use async_trait::async_trait;
use sessiontap_core::domain::PublicField;
use std::collections::BTreeSet;

/// Prints each update envelope as one JSON line, restricted to the configured
/// public fields.
pub struct StdoutSink {
    name: String,
    fields: BTreeSet<PublicField>,
}

impl StdoutSink {
    #[must_use]
    pub const fn new(name: String, fields: BTreeSet<PublicField>) -> Self {
        Self { name, fields }
    }
}

#[async_trait]
impl Sink for StdoutSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn deliver(&self, payload: &[u8]) -> DeliveryOutcome {
        let Some(projected) = project_fields(payload, &self.fields) else {
            return DeliveryOutcome::Reject;
        };
        println!("{}", String::from_utf8_lossy(&projected));
        DeliveryOutcome::Ack
    }
}
