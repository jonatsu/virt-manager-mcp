//! A VM's addresses, merged from libvirt's DHCP leases, the guest agent and the host's ARP table.

use std::net::IpAddr;

use serde::Serialize;
use virt::domain::{Domain, Interface};
use virt::sys;

/// Where an interface's addresses came from. The order is the order of trust: libvirt's own DHCP server hands out
/// leases, while the guest agent's answer and the ARP table are what the guest claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Lease,
    Agent,
    Arp,
}

impl Source {
    const ALL: [Self; 3] = [Self::Lease, Self::Agent, Self::Arp];

    fn libvirt(self) -> sys::virDomainInterfaceAddressesSource {
        match self {
            Self::Lease => sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_LEASE,
            Self::Agent => sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_AGENT,
            Self::Arp => sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_ARP,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Address {
    pub address: String,
    pub prefix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmInterface {
    /// The interface's name inside the guest, known only from the guest agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mac: String,
    pub source: Source,
    pub addresses: Vec<Address>,
}

/// One source's answer, or why it gave none.
pub type Answer = Result<Vec<Interface>, String>;

/// Asks each source in `sources` for `dom`'s addresses.
pub fn query(dom: &Domain, sources: &[Source]) -> Vec<(Source, Answer)> {
    sources
        .iter()
        .map(|&s| {
            let answer = dom
                .interface_addresses(s.libvirt(), 0)
                .map_err(|e| e.message().to_string());
            (s, answer)
        })
        .collect()
}

/// The sources a call may ask: every source, or only those that never reach the guest in read-only mode.
pub fn sources(read_only: bool) -> &'static [Source] {
    if read_only {
        &[Source::Lease, Source::Arp]
    } else {
        &Source::ALL
    }
}

/// Merges the answers per interface, matched by MAC address: for each interface the first source in trust order
/// that has a usable address wins. Loopback and IPv6 link-local addresses are dropped. Interfaces come in the order
/// of their first appearance.
pub fn merge(answers: &[(Source, Answer)]) -> Vec<VmInterface> {
    let mut macs: Vec<String> = Vec::new();
    for (_, answer) in answers {
        for iface in answer.iter().flatten() {
            let mac = iface.hwaddr.to_ascii_lowercase();
            if !macs.contains(&mac) {
                macs.push(mac);
            }
        }
    }
    let agent_name = |mac: &str| {
        answers
            .iter()
            .filter(|(s, _)| *s == Source::Agent)
            .flat_map(|(_, a)| a.iter().flatten())
            .find(|i| i.hwaddr.eq_ignore_ascii_case(mac))
            .map(|i| i.name.clone())
    };
    let mut merged = Vec::new();
    for mac in macs {
        let found = Source::ALL.iter().find_map(|&source| {
            let addresses: Vec<Address> = answers
                .iter()
                .filter(|(s, _)| *s == source)
                .flat_map(|(_, a)| a.iter().flatten())
                .filter(|i| i.hwaddr.eq_ignore_ascii_case(&mac))
                .flat_map(|i| &i.addrs)
                .filter(|a| usable(&a.addr))
                .map(|a| Address {
                    address: a.addr.clone(),
                    prefix: a.prefix,
                })
                .collect();
            (!addresses.is_empty()).then_some((source, addresses))
        });
        if let Some((source, addresses)) = found {
            merged.push(VmInterface {
                name: agent_name(&mac),
                mac,
                source,
                addresses,
            });
        }
    }
    merged
}

/// Whether `addr` is worth reporting: a parseable address that is neither loopback nor IPv6 link-local.
fn usable(addr: &str) -> bool {
    match addr.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => !a.is_loopback(),
        Ok(IpAddr::V6(a)) => !a.is_loopback() && (a.segments()[0] & 0xffc0) != 0xfe80,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use virt::domain::IPAddress;

    use super::*;

    fn iface(name: &str, mac: &str, addrs: &[&str]) -> Interface {
        Interface {
            name: name.into(),
            hwaddr: mac.into(),
            naddrs: addrs.len() as u64,
            addrs: addrs
                .iter()
                .map(|a| IPAddress {
                    typed: i64::from(a.contains(':')),
                    addr: (*a).into(),
                    prefix: if a.contains(':') { 64 } else { 24 },
                })
                .collect(),
        }
    }

    const MAC: &str = "52:54:00:7c:28:fc";

    #[test]
    fn a_lease_wins_over_what_the_guest_claims() {
        let answers = [
            (
                Source::Lease,
                Ok(vec![iface("vnet3", MAC, &["192.168.122.141"])]),
            ),
            (
                Source::Agent,
                Ok(vec![
                    iface("lo", "00:00:00:00:00:00", &["127.0.0.1", "::1"]),
                    iface("enp1s0", &MAC.to_uppercase(), &["192.168.122.1", "fe80::1"]),
                ]),
            ),
            (Source::Arp, Ok(vec![iface("vnet3", MAC, &["10.0.0.9"])])),
        ];
        assert_eq!(
            merge(&answers),
            [VmInterface {
                name: Some("enp1s0".into()),
                mac: MAC.into(),
                source: Source::Lease,
                addresses: vec![Address {
                    address: "192.168.122.141".into(),
                    prefix: 24
                }],
            }]
        );
    }

    #[test]
    fn an_interface_without_a_lease_keeps_the_agents_or_arps_answer() {
        let static_mac = "52:54:00:00:00:02";
        let arp_mac = "52:54:00:00:00:03";
        let answers = [
            (Source::Lease, Ok(vec![])),
            (
                Source::Agent,
                Ok(vec![iface(
                    "eth1",
                    static_mac,
                    &["10.1.0.5", "fd00::5", "fe80::5"],
                )]),
            ),
            (
                Source::Arp,
                Ok(vec![iface("vnet9", arp_mac, &["10.2.0.7"])]),
            ),
        ];
        let merged = merge(&answers);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, Source::Agent);
        assert_eq!(merged[0].name.as_deref(), Some("eth1"));
        let kept: Vec<_> = merged[0]
            .addresses
            .iter()
            .map(|a| a.address.as_str())
            .collect();
        assert_eq!(kept, ["10.1.0.5", "fd00::5"]);
        assert_eq!(merged[1].source, Source::Arp);
        assert_eq!(merged[1].name, None);
    }

    #[test]
    fn failed_sources_and_only_loopback_give_an_empty_list() {
        let answers = [
            (Source::Lease, Err("no leases".to_string())),
            (
                Source::Agent,
                Ok(vec![iface("lo", "00:00:00:00:00:00", &["127.0.0.1"])]),
            ),
            (Source::Arp, Err("no arp".to_string())),
        ];
        assert!(merge(&answers).is_empty());
    }

    #[test]
    fn read_only_mode_never_asks_the_agent() {
        assert!(!sources(true).contains(&Source::Agent));
        assert_eq!(sources(false), Source::ALL);
    }
}
