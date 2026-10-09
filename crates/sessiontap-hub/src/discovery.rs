//! DNS-SD announcement of the wildcard remote listener (`remote.discovery`).
//! The announcement only adds addresses for paired devices to try: it names
//! no hub, grants no trust, and lives only while the listener is bound.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceInfo};
use tokio_util::sync::CancellationToken;

use crate::config::{ListenMode, RemoteConfig};
use crate::endpoints::{self, InterfaceAddr};

/// DNS-SD service type announced by hubs.
pub const SERVICE_TYPE: &str = "_sessiontap._tcp.local.";

/// How often the announced address set is compared with the host's.
pub const ADDRESS_POLL: Duration = Duration::from_secs(10);

/// How long shutdown waits for each goodbye to go out.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(1);

/// One announced service: random names, the wildcard's port, and the
/// addresses the wildcard is reachable at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Instance label, `st-<12 hex>`.
    pub instance: String,
    /// SRV target, `st-<12 hex>.local.`.
    pub host: String,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
}

impl Record {
    /// TXT record contents.
    pub const TXT: [(&'static str, &'static str); 1] = [("v", "1")];

    /// Full DNS-SD instance name.
    #[must_use]
    pub fn fullname(&self) -> String {
        format!("{}.{SERVICE_TYPE}", self.instance)
    }
}

/// `st-` followed by 12 random hex digits.
#[must_use]
pub fn random_label() -> String {
    let bytes: [u8; 6] = rand::random();
    format!("st-{}", hex::encode(bytes))
}

/// Where announcements go; the production backend is mDNS.
pub trait Publisher: Send + Sync {
    /// Publishes `record`, replacing an earlier record with the same name.
    fn register(&self, record: &Record) -> Result<(), String>;
    /// Withdraws `record` with goodbye packets.
    fn unregister(&self, record: &Record) -> Result<(), String>;
    /// Releases the backend after every record is withdrawn.
    fn shutdown(&self) {}
}

/// Announces one wildcard listener. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct Discovery {
    inner: Arc<Inner>,
}

struct Inner {
    wildcard: IpAddr,
    instance: String,
    host: String,
    publisher: Box<dyn Publisher>,
    interfaces: fn() -> Vec<InterfaceAddr>,
    poll: Duration,
    /// The record currently published, if any.
    current: Mutex<Option<Record>>,
    /// Failure kinds already logged.
    logged: Mutex<HashSet<&'static str>>,
}

impl Discovery {
    /// Discovery for `remote`, or `None` when it is off. Opening the mDNS
    /// socket is deferred to the first announcement, so a hub with discovery
    /// off sends and answers no mDNS packets.
    #[must_use]
    pub fn from_config(remote: &RemoteConfig) -> Option<Self> {
        if !remote.discovery {
            return None;
        }
        let ListenMode::Wildcard(wildcard) = remote.listen_mode() else {
            return None;
        };
        Some(Self::new(
            wildcard.ip(),
            Box::new(MdnsPublisher::new(wildcard.ip())),
            endpoints::system_interfaces,
            ADDRESS_POLL,
        ))
    }

    /// Discovery with random names for this start.
    #[must_use]
    pub fn new(
        wildcard: IpAddr,
        publisher: Box<dyn Publisher>,
        interfaces: fn() -> Vec<InterfaceAddr>,
        poll: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                wildcard,
                instance: random_label(),
                host: format!("{}.local.", random_label()),
                publisher,
                interfaces,
                poll,
                current: Mutex::new(None),
                logged: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// The record currently announced, if any.
    #[must_use]
    pub fn current(&self) -> Option<Record> {
        self.inner.current.lock().unwrap().clone()
    }

    /// Announces `bound` until the returned guard drops, following address
    /// changes every poll interval.
    #[must_use]
    pub fn announce(&self, bound: SocketAddr) -> Announcement {
        let stop = CancellationToken::new();
        let inner = Arc::clone(&self.inner);
        let token = stop.clone();
        let port = bound.port();
        tokio::spawn(async move {
            loop {
                inner.refresh(port);
                tokio::select! {
                    () = token.cancelled() => break,
                    () = tokio::time::sleep(inner.poll) => {}
                }
            }
            inner.withdraw();
        });
        Announcement { stop }
    }

    /// Sends goodbyes for the current record and releases the backend.
    pub fn shutdown(&self) {
        self.inner.withdraw();
        self.inner.publisher.shutdown();
    }
}

impl Inner {
    /// Publishes the current address set when it differs from the last one.
    fn refresh(&self, port: u16) {
        let addresses = endpoints::wildcard_addresses(self.wildcard, &(self.interfaces)());
        let mut current = self.current.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|record| record.port == port && record.addresses == addresses)
        {
            return;
        }
        if let Some(old) = current.take() {
            if let Err(error) = self.publisher.unregister(&old) {
                self.log_once("withdraw", &error);
            }
        }
        if addresses.is_empty() {
            return;
        }
        let record = Record {
            instance: self.instance.clone(),
            host: self.host.clone(),
            port,
            addresses,
        };
        match self.publisher.register(&record) {
            Ok(()) => *current = Some(record),
            Err(error) => self.log_once("announce", &error),
        }
    }

    fn withdraw(&self) {
        if let Some(old) = self.current.lock().unwrap().take() {
            if let Err(error) = self.publisher.unregister(&old) {
                self.log_once("withdraw", &error);
            }
        }
    }

    fn log_once(&self, kind: &'static str, error: &str) {
        if self.logged.lock().unwrap().insert(kind) {
            eprintln!("sessiontap-hub: discovery cannot {kind}: {error}");
        }
    }
}

/// A live announcement; dropping it withdraws the record.
pub struct Announcement {
    stop: CancellationToken,
}

impl Drop for Announcement {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// mDNS backend over `mdns-sd`, opened on first use. Records carry explicit
/// addresses; loopback, container and VM bridges, and IPv6 interfaces under
/// an IPv4 wildcard are disabled.
pub struct MdnsPublisher {
    wildcard: IpAddr,
    daemon: Mutex<Option<ServiceDaemon>>,
}

impl MdnsPublisher {
    #[must_use]
    pub fn new(wildcard: IpAddr) -> Self {
        Self {
            wildcard,
            daemon: Mutex::new(None),
        }
    }

    fn with_daemon<T>(
        &self,
        action: impl FnOnce(&ServiceDaemon) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut daemon = self.daemon.lock().unwrap();
        if daemon.is_none() {
            let opened = ServiceDaemon::new().map_err(|error| error.to_string())?;
            let ipv4_only = self.wildcard.is_ipv4();
            opened
                .disable_interface(IfKind::Predicate(IfPredicate::new(move |interface| {
                    interface.is_loopback()
                        || endpoints::ignored_interface(&interface.name)
                        || (ipv4_only && interface.ip().is_ipv6())
                })))
                .map_err(|error| error.to_string())?;
            *daemon = Some(opened);
        }
        action(daemon.as_ref().expect("opened above"))
    }
}

impl Publisher for MdnsPublisher {
    fn register(&self, record: &Record) -> Result<(), String> {
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &record.instance,
            &record.host,
            record.addresses.as_slice(),
            record.port,
            &Record::TXT[..],
        )
        .map_err(|error| error.to_string())?;
        self.with_daemon(|daemon| daemon.register(info).map_err(|error| error.to_string()))
    }

    fn unregister(&self, record: &Record) -> Result<(), String> {
        self.with_daemon(|daemon| {
            let done = daemon
                .unregister(&record.fullname())
                .map_err(|error| error.to_string())?;
            // goodbye packets go out before the status arrives
            let _ = done.recv_timeout(GOODBYE_TIMEOUT);
            Ok(())
        })
    }

    fn shutdown(&self) {
        if let Some(daemon) = self.daemon.lock().unwrap().take() {
            if let Ok(done) = daemon.shutdown() {
                let _ = done.recv_timeout(GOODBYE_TIMEOUT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        events: Mutex<Vec<(&'static str, Record)>>,
        fail: bool,
    }

    impl Publisher for Arc<Fake> {
        fn register(&self, record: &Record) -> Result<(), String> {
            if self.fail {
                return Err("no socket".into());
            }
            self.events
                .lock()
                .unwrap()
                .push(("register", record.clone()));
            Ok(())
        }
        fn unregister(&self, record: &Record) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(("unregister", record.clone()));
            Ok(())
        }
    }

    fn interface(name: &str, ip: &str) -> InterfaceAddr {
        InterfaceAddr {
            name: name.into(),
            ip: ip.parse().unwrap(),
            up: true,
        }
    }

    fn host() -> Vec<InterfaceAddr> {
        vec![
            interface("lo", "127.0.0.1"),
            interface("wlan0", "192.168.0.165"),
            interface("wlan0", "2001:db8::5"),
            interface("wlan0", "fe80::1"),
            interface("docker0", "172.17.0.1"),
            interface("eth1", "169.254.3.4"),
        ]
    }

    fn moved() -> Vec<InterfaceAddr> {
        vec![interface("wlan0", "192.168.0.170")]
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|ip| ip.parse().unwrap()).collect()
    }

    fn discovery(
        wildcard: &str,
        fake: &Arc<Fake>,
        interfaces: fn() -> Vec<InterfaceAddr>,
    ) -> Discovery {
        Discovery::new(
            wildcard.parse().unwrap(),
            Box::new(Arc::clone(fake)),
            interfaces,
            Duration::from_millis(20),
        )
    }

    fn remote(listen: &str, discovery: bool) -> RemoteConfig {
        RemoteConfig {
            name: Some("Laptop".into()),
            listen: vec![listen.into()],
            advertise: vec!["macbook.tailnet.ts.net:8932".into()],
            control: false,
            discovery,
        }
    }

    #[test]
    fn off_means_no_discovery() {
        assert!(Discovery::from_config(&remote("0.0.0.0:8932", false)).is_none());
        assert!(Discovery::from_config(&remote("0.0.0.0:8932", true)).is_some());
    }

    #[tokio::test]
    async fn ipv4_wildcard_announces_a_records_only() {
        let fake = Arc::new(Fake::default());
        let discovery = discovery("0.0.0.0", &fake, host);
        let _guard = discovery.announce("0.0.0.0:8932".parse().unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        let record = discovery.current().unwrap();
        assert_eq!(record.port, 8932);
        assert_eq!(record.addresses, ips(&["192.168.0.165"]));
        assert_eq!(Record::TXT, [("v", "1")]);
        assert_eq!(
            record.fullname(),
            format!("{}._sessiontap._tcp.local.", record.instance)
        );
    }

    #[tokio::test]
    async fn ipv6_wildcard_adds_global_aaaa_only() {
        let fake = Arc::new(Fake::default());
        let discovery = discovery("::", &fake, host);
        let _guard = discovery.announce("[::]:8932".parse().unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(
            discovery.current().unwrap().addresses,
            ips(&["192.168.0.165", "2001:db8::5"])
        );
    }

    #[test]
    fn names_are_random_and_anonymous() {
        let fake = Arc::new(Fake::default());
        let first = discovery("0.0.0.0", &fake, host);
        let second = discovery("0.0.0.0", &fake, host);
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        for names in [&first.inner, &second.inner] {
            assert!(names.instance.starts_with("st-") && names.instance.len() == 15);
            assert!(names.host.starts_with("st-") && names.host.ends_with(".local."));
            for name in [&names.instance, &names.host] {
                assert!(!name.contains("Laptop"));
                let hostname = hostname.trim();
                assert!(hostname.is_empty() || !name.contains(hostname));
            }
        }
        assert_ne!(first.inner.instance, second.inner.instance);
        assert_ne!(first.inner.host, second.inner.host);
    }

    #[tokio::test]
    async fn address_change_reannounces() {
        static MOVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn interfaces() -> Vec<InterfaceAddr> {
            if MOVED.load(std::sync::atomic::Ordering::SeqCst) {
                moved()
            } else {
                host()
            }
        }
        let fake = Arc::new(Fake::default());
        let discovery = discovery("0.0.0.0", &fake, interfaces);
        let _guard = discovery.announce("0.0.0.0:8932".parse().unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        MOVED.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            discovery.current().unwrap().addresses,
            ips(&["192.168.0.170"])
        );
        let events = fake.events.lock().unwrap();
        let kinds: Vec<_> = events
            .iter()
            .map(|(kind, r)| (*kind, r.addresses.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("register", ips(&["192.168.0.165"])),
                ("unregister", ips(&["192.168.0.165"])),
                ("register", ips(&["192.168.0.170"])),
            ]
        );
    }

    #[tokio::test]
    async fn unbound_withdraws_and_shutdown_says_goodbye() {
        let fake = Arc::new(Fake::default());
        let discovery = discovery("0.0.0.0", &fake, host);
        assert!(discovery.current().is_none());
        let guard = discovery.announce("0.0.0.0:8932".parse().unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(discovery.current().is_some());
        drop(guard);
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(discovery.current().is_none());
        let _guard = discovery.announce("0.0.0.0:8932".parse().unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        discovery.shutdown();
        assert!(discovery.current().is_none());
        let kinds: Vec<_> = fake
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, _)| *kind)
            .collect();
        assert_eq!(
            kinds,
            vec!["register", "unregister", "register", "unregister"]
        );
    }

    #[tokio::test]
    async fn failures_are_logged_once_and_keep_running() {
        let fake = Arc::new(Fake {
            fail: true,
            ..Fake::default()
        });
        let discovery = discovery("0.0.0.0", &fake, host);
        let _guard = discovery.announce("0.0.0.0:8932".parse().unwrap());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(discovery.current().is_none());
        assert_eq!(discovery.inner.logged.lock().unwrap().len(), 1);
    }
}
