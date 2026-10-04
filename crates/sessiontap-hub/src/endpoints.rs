//! Pairing endpoint hints: where a device should try to reach the hub.
//! Hints carry reachability only; trust comes from the pinned hub ID.

use std::net::{IpAddr, SocketAddr};

use nix::{ifaddrs::getifaddrs, net::if_::InterfaceFlags};

use crate::config::{ListenMode, RemoteConfig};

/// Interface name prefixes of container and VM bridges never offered as hints.
const IGNORED_INTERFACE_PREFIXES: [&str; 4] = ["docker", "veth", "virbr", "br-"];

/// One address assigned to a host interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddr {
    pub name: String,
    pub ip: IpAddr,
    pub up: bool,
}

/// Endpoint hints for a configuration and an interface list. Explicit mode
/// ignores `interfaces`; wildcard mode keeps the usable interface addresses.
/// `remote.advertise` comes last, and duplicates keep the first occurrence.
#[must_use]
pub fn endpoint_hints(remote: &RemoteConfig, interfaces: &[InterfaceAddr]) -> Vec<String> {
    let addresses: Vec<String> = match remote.listen_mode() {
        ListenMode::Explicit(addresses) => addresses.iter().map(ToString::to_string).collect(),
        ListenMode::Wildcard(wildcard) => interfaces
            .iter()
            .filter(|interface| usable(interface, wildcard.ip()))
            .map(|interface| SocketAddr::new(interface.ip, wildcard.port()).to_string())
            .collect(),
    };
    let mut hints: Vec<String> = Vec::new();
    for hint in addresses
        .into_iter()
        .chain(remote.advertise.iter().cloned())
    {
        if !hints.contains(&hint) {
            hints.push(hint);
        }
    }
    hints
}

fn usable(interface: &InterfaceAddr, wildcard: IpAddr) -> bool {
    if !interface.up
        || IGNORED_INTERFACE_PREFIXES
            .iter()
            .any(|prefix| interface.name.starts_with(prefix))
    {
        return false;
    }
    match (interface.ip, wildcard) {
        (IpAddr::V4(ip), _) => !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified(),
        (IpAddr::V6(ip), IpAddr::V6(_)) => {
            !ip.is_loopback() && !ip.is_unicast_link_local() && !ip.is_unspecified()
        }
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

/// Addresses of the host's interfaces in the order the system reports them.
/// An enumeration error is logged and yields an empty list.
#[must_use]
pub fn system_interfaces() -> Vec<InterfaceAddr> {
    let addresses = match getifaddrs() {
        Ok(addresses) => addresses,
        Err(error) => {
            eprintln!("sessiontap-hub: cannot list network interfaces: {error}");
            return Vec::new();
        }
    };
    addresses
        .filter_map(|entry| {
            let address = entry.address?;
            let ip = if let Some(v4) = address.as_sockaddr_in() {
                IpAddr::V4(v4.ip())
            } else if let Some(v6) = address.as_sockaddr_in6() {
                IpAddr::V6(v6.ip())
            } else {
                return None;
            };
            Some(InterfaceAddr {
                name: entry.interface_name,
                ip,
                up: entry.flags.contains(InterfaceFlags::IFF_UP),
            })
        })
        .collect()
}

/// Endpoint hints for the host as it is now.
#[must_use]
pub fn current_endpoints(remote: &RemoteConfig) -> Vec<String> {
    endpoints_with(remote, system_interfaces)
}

/// Endpoint hints, listing interfaces through `interfaces` only in wildcard
/// mode.
pub fn endpoints_with(
    remote: &RemoteConfig,
    interfaces: impl FnOnce() -> Vec<InterfaceAddr>,
) -> Vec<String> {
    match remote.listen_mode() {
        ListenMode::Explicit(_) => endpoint_hints(remote, &[]),
        ListenMode::Wildcard(_) => endpoint_hints(remote, &interfaces()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(listen: &[&str], advertise: &[&str]) -> RemoteConfig {
        let remote = RemoteConfig {
            name: None,
            listen: listen.iter().map(|entry| (*entry).to_owned()).collect(),
            advertise: advertise.iter().map(|entry| (*entry).to_owned()).collect(),
            control: false,
        };
        remote.validate().unwrap();
        remote
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
            interface("lo", "::1"),
            interface("wlan0", "192.168.0.165"),
            interface("wlan0", "fe80::1"),
            interface("wlan0", "2001:db8::5"),
            interface("tailscale0", "100.64.0.7"),
            interface("docker0", "172.17.0.1"),
            interface("veth1a2b", "172.17.0.2"),
            interface("virbr0", "192.168.122.1"),
            interface("br-3f2e", "172.18.0.1"),
            interface("eth1", "169.254.3.4"),
            InterfaceAddr {
                up: false,
                ..interface("eth2", "10.0.0.9")
            },
        ]
    }

    #[test]
    fn explicit_mode_ignores_interfaces() {
        let remote = remote(
            &["100.64.0.7:8932", "192.168.1.20:8932"],
            &["macbook.tailnet.ts.net:8932"],
        );
        assert_eq!(
            endpoint_hints(&remote, &host()),
            vec![
                "100.64.0.7:8932",
                "192.168.1.20:8932",
                "macbook.tailnet.ts.net:8932"
            ]
        );
    }

    #[test]
    fn ipv4_wildcard_keeps_usable_ipv4_addresses() {
        let remote = remote(&["0.0.0.0:8932"], &["macbook.tailnet.ts.net:8932"]);
        assert_eq!(
            endpoint_hints(&remote, &host()),
            vec![
                "192.168.0.165:8932",
                "100.64.0.7:8932",
                "macbook.tailnet.ts.net:8932"
            ]
        );
    }

    #[test]
    fn ipv6_wildcard_keeps_global_ipv6_and_ipv4() {
        let remote = remote(&["[::]:8932"], &[]);
        assert_eq!(
            endpoint_hints(&remote, &host()),
            vec![
                "192.168.0.165:8932",
                "[2001:db8::5]:8932",
                "100.64.0.7:8932"
            ]
        );
    }

    #[test]
    fn duplicates_keep_the_first_occurrence() {
        let remote = remote(&["0.0.0.0:8932"], &["100.64.0.7:8932", "hub:8932"]);
        let interfaces = vec![
            interface("tailscale0", "100.64.0.7"),
            interface("wlan0", "192.168.0.165"),
            interface("wlan1", "192.168.0.165"),
        ];
        assert_eq!(
            endpoint_hints(&remote, &interfaces),
            vec!["100.64.0.7:8932", "192.168.0.165:8932", "hub:8932"]
        );
    }

    #[test]
    fn wildcard_without_usable_addresses_is_empty() {
        let remote = remote(&["0.0.0.0:8932"], &[]);
        assert!(endpoint_hints(&remote, &[interface("lo", "127.0.0.1")]).is_empty());
    }

    #[test]
    fn system_interfaces_smoke() {
        let _ = system_interfaces();
        let remote = remote(&["127.0.0.1:8932"], &["hub:8932"]);
        assert_eq!(current_endpoints(&remote), endpoint_hints(&remote, &[]));
    }
}
