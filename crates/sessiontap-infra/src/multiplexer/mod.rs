//! Backend-neutral multiplexer interface and backend dispatch.

mod control;
mod tmux;

use anyhow::Result;
pub use sessiontap_core::domain::MultiplexerBackend;
use sessiontap_core::{
    domain::{Capabilities, MultiplexerMetadata},
    terminal::{Cursor, EndReason, Key},
};
use std::{collections::BTreeMap, sync::Arc};
pub use tmux::TmuxAdapter;
use tokio::sync::mpsc;

/// Pane contents and state at one point in the pane's output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: Cursor,
    pub alternate_screen: bool,
    pub in_mode: bool,
    /// Scrollback and visible screen as raw terminal bytes.
    pub data: Vec<u8>,
}

/// Backend-neutral live pane events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// Taken in order with output: later `Output` follows it exactly.
    Snapshot(PaneSnapshot),
    Output(Vec<u8>),
    /// The backend dropped output; a fresh snapshot is needed.
    Resync,
    ModeChanged(bool),
    Resized,
    Closed(EndReason),
}

/// Control side of an open stream. Dropping the last handle closes the
/// backend connection.
pub trait PaneStreamControl: Send + Sync {
    /// Asks for a fresh [`PaneEvent::Snapshot`] in stream order.
    fn request_snapshot(&self) -> Result<()>;
}

/// An open live pane stream; the first event is a snapshot.
pub struct PaneStream {
    pub events: mpsc::UnboundedReceiver<PaneEvent>,
    pub control: Arc<dyn PaneStreamControl>,
}

/// Input guard inputs read from the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneState {
    pub foreground_pgid: Option<u32>,
    pub in_mode: bool,
}

pub trait MultiplexerAdapter {
    fn inspect(&self) -> Result<Option<MultiplexerMetadata>>;
    fn capture(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<String>;
    /// Opens a live, output-only stream of the pane. It never resizes or
    /// writes to the pane.
    fn open_stream(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<PaneStream>;
    /// Sends named keys in the pane's key encoding and characters literally.
    fn send_keys(
        &self,
        expected: &MultiplexerMetadata,
        process_pid: u32,
        keys: &[Key],
    ) -> Result<()>;
    /// Pastes text literally (bracketed when the application enabled it),
    /// followed by Enter when asked.
    fn paste(
        &self,
        expected: &MultiplexerMetadata,
        process_pid: u32,
        text: &str,
        enter: bool,
    ) -> Result<()>;
    fn pane_state(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<PaneState>;
    fn capabilities(&self, present: bool) -> Capabilities {
        Capabilities {
            capture: present,
            send_input: present,
            usage: false,
            terminal: false,
            terminal_policy: Default::default(),
        }
    }
}

pub type SharedMultiplexer = Arc<dyn MultiplexerAdapter + Send + Sync>;

/// The recorded backend has no registered adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unsupported multiplexer backend '{0}'")]
pub struct UnsupportedBackend(pub MultiplexerBackend);

/// Maps each recorded backend to the adapter that serves it.
#[derive(Clone)]
pub struct MultiplexerRegistry {
    adapters: BTreeMap<MultiplexerBackend, SharedMultiplexer>,
}

impl Default for MultiplexerRegistry {
    fn default() -> Self {
        Self::empty().with_adapter(MultiplexerBackend::Tmux, Arc::new(TmuxAdapter))
    }
}

impl MultiplexerRegistry {
    /// Registry with every built-in backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with no adapters; every backend is unsupported.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_adapter(mut self, backend: MultiplexerBackend, adapter: SharedMultiplexer) -> Self {
        self.adapters.insert(backend, adapter);
        self
    }

    #[must_use]
    pub fn for_backend(&self, backend: MultiplexerBackend) -> Option<&dyn MultiplexerAdapter> {
        self.adapters
            .get(&backend)
            .map(|adapter| adapter.as_ref() as &dyn MultiplexerAdapter)
    }

    /// Shared handle to the adapter for `backend`, for long-lived streams.
    #[must_use]
    pub fn shared(&self, backend: MultiplexerBackend) -> Option<SharedMultiplexer> {
        self.adapters.get(&backend).cloned()
    }

    /// Adapter for `backend`, or a typed [`UnsupportedBackend`] error.
    pub fn require(&self, backend: MultiplexerBackend) -> Result<&dyn MultiplexerAdapter> {
        Ok(self
            .for_backend(backend)
            .ok_or(UnsupportedBackend(backend))?)
    }

    /// Returns metadata from the first backend that reports the current
    /// process runs inside it.
    pub fn detect(&self) -> Result<Option<MultiplexerMetadata>> {
        for adapter in self.adapters.values() {
            if let Some(metadata) = adapter.inspect()? {
                return Ok(Some(metadata));
            }
        }
        Ok(None)
    }

    /// Capabilities for detected metadata; none when not in a multiplexer.
    #[must_use]
    pub fn capabilities(&self, metadata: Option<&MultiplexerMetadata>) -> Capabilities {
        metadata
            .and_then(|metadata| self.for_backend(metadata.backend))
            .map(|adapter| adapter.capabilities(true))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_reports_unsupported_backend() {
        let registry = MultiplexerRegistry::empty();
        let error = registry.require(MultiplexerBackend::Tmux).err().unwrap();
        assert_eq!(
            error.downcast_ref::<UnsupportedBackend>(),
            Some(&UnsupportedBackend(MultiplexerBackend::Tmux))
        );
        assert!(registry.detect().unwrap().is_none());
        assert_eq!(registry.capabilities(None), Capabilities::default());
    }

    #[test]
    fn default_registry_serves_tmux() {
        assert!(
            MultiplexerRegistry::new()
                .for_backend(MultiplexerBackend::Tmux)
                .is_some()
        );
    }
}
