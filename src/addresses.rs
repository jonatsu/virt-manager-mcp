//! A VM's addresses, merged from libvirt's DHCP leases, the guest agent and the host's ARP table.

use std::net::IpAddr;

use serde::Serialize;
use serde_json::{Value, json};
use virt::domain::{Domain, IPAddress, Interface};
use virt::sys;

use crate::agent::Agent;
use crate::config::Config;

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

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lease => "lease",
            Self::Agent => "agent",
            Self::Arp => "arp",
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

/// Asks each source in `sources` for `dom`'s addresses. Leases and ARP come from libvirt, which takes each MAC
/// address from the VM's definition. The guest agent is asked through the server's own agent channel instead of
/// libvirt's agent source, because `virt` 0.4.3 converts an interface's MAC address without checking for NULL, which
/// libvirt allows for agent answers, and would crash the server.
pub fn query(dom: &Domain, agent_timeout_secs: i32, sources: &[Source]) -> Vec<(Source, Answer)> {
    sources
        .iter()
        .map(|&s| {
            let answer = match s {
                Source::Lease => libvirt(dom, sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_LEASE),
                Source::Arp => libvirt(dom, sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_ARP),
                Source::Agent => Agent::new(dom, agent_timeout_secs)
                    .call("guest-network-get-interfaces", None)
                    .map(|v| agent_interfaces(&v))
                    .map_err(|e| format!("{e:#}")),
            };
            (s, answer)
        })
        .collect()
}

fn libvirt(dom: &Domain, source: sys::virDomainInterfaceAddressesSource) -> Answer {
    // libvirt copies each NIC's host-side device name into the answer (`virDomainNetDHCPInterfaces` and
    // `virDomainNetARPInterfaces` in libvirt 12.0.0), NULL for a NIC without one, such as a passed-through device.
    // `virt` 0.4.3 reads that name without a NULL check, so the call is made only when every NIC has a name.
    let xml = dom.get_xml_desc(0).map_err(|e| e.message().to_string())?;
    if !every_nic_has_a_device_name(&xml) {
        return Err(
            "skipped: a network interface has no host-side device name, which this server cannot read safely"
                .to_string(),
        );
    }
    dom.interface_addresses(source, 0)
        .map_err(|e| e.message().to_string())
}

/// Whether every `<interface>` in a domain's live XML has a `<target dev="...">`.
fn every_nic_has_a_device_name(xml: &str) -> bool {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return false;
    };
    doc.descendants()
        .filter(|n| {
            n.has_tag_name("interface") && n.parent().is_some_and(|p| p.has_tag_name("devices"))
        })
        .all(|nic| {
            nic.children()
                .find(|c| c.has_tag_name("target"))
                .and_then(|t| t.attribute("dev"))
                .is_some_and(|d| !d.is_empty())
        })
}

/// The interfaces in a `guest-network-get-interfaces` reply. An interface without a hardware address cannot be
/// matched to the VM's NICs and is left out.
fn agent_interfaces(reply: &Value) -> Vec<Interface> {
    let Some(list) = reply.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|i| {
            let hwaddr = i.get("hardware-address")?.as_str()?.to_string();
            let addrs: Vec<IPAddress> = i
                .get("ip-addresses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    Some(IPAddress {
                        typed: i64::from(a.get("ip-address-type")?.as_str()? == "ipv6"),
                        addr: a.get("ip-address")?.as_str()?.to_string(),
                        prefix: a.get("prefix")?.as_u64()?,
                    })
                })
                .collect();
            Some(Interface {
                name: i
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                hwaddr,
                naddrs: addrs.len() as u64,
                addrs,
            })
        })
        .collect()
}

/// The sources a call may ask under `config`: every source, or in read-only mode only those that never reach the
/// guest.
pub fn sources(config: &Config) -> &'static [Source] {
    if config.read_only {
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
        Ok(IpAddr::V4(a)) => !a.is_loopback() && !a.is_unspecified(),
        Ok(IpAddr::V6(a)) => {
            !a.is_loopback()
                && !a.is_unspecified()
                && (a.segments()[0] & 0xffc0) != 0xfe80
                && !a.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

/// The tool's reply: the merged interfaces, and the sources that failed with their reasons.
pub fn reply(answers: &[(Source, Answer)]) -> Value {
    let unavailable: serde_json::Map<String, Value> = answers
        .iter()
        .filter_map(|(source, a)| {
            a.as_ref()
                .err()
                .map(|e| (source.as_str().to_string(), json!(e)))
        })
        .collect();
    json!({ "interfaces": merge(answers), "unavailable": unavailable })
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
        let read_only: Config = toml::from_str("read_only = true").unwrap();
        assert!(!sources(&read_only).contains(&Source::Agent));
        assert_eq!(sources(&Config::default()), Source::ALL);
    }

    #[test]
    fn agent_interfaces_without_a_mac_are_left_out() {
        let reply = json!([
            {"name": "lo", "hardware-address": "00:00:00:00:00:00",
             "ip-addresses": [{"ip-address-type": "ipv4", "ip-address": "127.0.0.1", "prefix": 8}]},
            {"name": "wg0", "ip-addresses": [{"ip-address-type": "ipv4", "ip-address": "10.9.0.2", "prefix": 32}]},
            {"name": "enp1s0", "hardware-address": MAC,
             "ip-addresses": [{"ip-address-type": "ipv6", "ip-address": "fd00::7", "prefix": 64},
                              {"ip-address-type": "ipv4", "prefix": 24}]}
        ]);
        let interfaces = agent_interfaces(&reply);
        let names: Vec<_> = interfaces.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["lo", "enp1s0"]);
        assert_eq!(
            interfaces[1].addrs.len(),
            1,
            "an address without ip-address is dropped"
        );
        assert_eq!(interfaces[1].addrs[0].typed, 1);
        assert!(agent_interfaces(&json!({"unexpected": true})).is_empty());
    }

    #[test]
    fn unusable_addresses_are_dropped() {
        for addr in [
            "127.0.0.1",
            "0.0.0.0",
            "::",
            "::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "nonsense",
        ] {
            assert!(!usable(addr), "{addr} kept");
        }
        for addr in [
            "192.168.122.141",
            "fd00::7",
            "2001:db8::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(usable(addr), "{addr} dropped");
        }
    }

    #[test]
    fn libvirt_sources_need_every_nic_named() {
        let named = r#"<domain><devices>
            <interface type="network"><mac address="52:54:00:00:00:01"/><target dev="vnet3"/></interface>
            <hostdev mode="subsystem"/></devices></domain>"#;
        assert!(every_nic_has_a_device_name(named));
        let passthrough = r#"<domain><devices>
            <interface type="network"><target dev="vnet3"/></interface>
            <interface type="hostdev"><mac address="52:54:00:00:00:02"/></interface></devices></domain>"#;
        assert!(!every_nic_has_a_device_name(passthrough));
        assert!(every_nic_has_a_device_name("<domain><devices/></domain>"));
        assert!(!every_nic_has_a_device_name("not xml"));
    }

    #[test]
    fn the_reply_names_failed_sources() {
        let answers = [
            (
                Source::Lease,
                Ok(vec![iface("vnet3", MAC, &["192.168.122.141"])]),
            ),
            (
                Source::Agent,
                Err("guest agent is not connected".to_string()),
            ),
        ];
        let reply = reply(&answers);
        assert_eq!(
            reply["unavailable"],
            json!({"agent": "guest agent is not connected"})
        );
        assert_eq!(reply["interfaces"][0]["source"], "lease");
    }
}
