//! mDNS / DNS-SD: every node advertises `_aura._tcp.local.` with its device id,
//! name and group, and browses for the others.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use anyhow::Result;
use aura_core::{DeviceId, GroupId, PROTOCOL_VERSION};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;

pub const SERVICE_TYPE: &str = "_aura._tcp.local.";

/// What this node advertises.
#[derive(Debug, Clone)]
pub struct Advert {
    pub device_id: DeviceId,
    pub name: String,
    pub group: Option<(GroupId, String)>,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub device_id: DeviceId,
    pub name: String,
    pub group: Option<(GroupId, String)>,
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Found(Discovered),
    Lost(DeviceId),
}

pub struct Discovery {
    daemon: ServiceDaemon,
    registered: Option<String>,
    web_registered: Option<String>,
    /// What we currently advertise, so [`Discovery::refresh`] can re-register it.
    advert: Option<Advert>,
    web_port: Option<u16>,
    /// The addresses our records point to (see [`lan_addresses`]).
    addrs: Vec<IpAddr>,
}

impl Discovery {
    /// Starts advertising (if `advert` is given) and browsing. Events about other
    /// devices arrive on the returned channel.
    pub fn start(advert: Option<&Advert>) -> Result<(Self, mpsc::Receiver<DiscoveryEvent>)> {
        let daemon = ServiceDaemon::new()?;
        // Phones and other PCs never reach us over loopback, and announcing there too
        // gives the daemon a second copy of our names to "conflict" with.
        daemon.disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])?;
        let browse = daemon.browse(SERVICE_TYPE)?;
        let me = advert.map(|a| a.device_id);
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            // fullname -> device id, to translate removals.
            let mut names: HashMap<String, DeviceId> = HashMap::new();
            while let Ok(ev) = browse.recv_async().await {
                let out = match ev {
                    ServiceEvent::ServiceResolved(svc) => {
                        let Some(found) = parse(&svc) else { continue };
                        if Some(found.device_id) == me {
                            continue;
                        }
                        names.insert(svc.fullname.clone(), found.device_id);
                        DiscoveryEvent::Found(found)
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => match names.remove(&fullname) {
                        Some(id) => DiscoveryEvent::Lost(id),
                        None => continue,
                    },
                    _ => continue,
                };
                if tx.send(out).await.is_err() {
                    break;
                }
            }
        });
        let mut d = Self {
            daemon,
            registered: None,
            web_registered: None,
            advert: None,
            web_port: None,
            addrs: lan_addresses(),
        };
        if let Some(advert) = advert {
            d.advertise(advert)?;
        }
        Ok((d, rx))
    }

    /// Claims (`Some(port)`) or releases (`None`) the `aura.local` host name for the
    /// phone page. Only the fofoqueiro holds it, so the name follows the role.
    pub fn advertise_web(&mut self, port: Option<u16>) -> Result<()> {
        self.web_port = port;
        if let Some(old) = self.web_registered.take() {
            let _ = self.daemon.unregister(&old);
        }
        let Some(port) = port else {
            return Ok(());
        };
        let info = self.service_info(
            "_https._tcp.local.",
            "Aura Clipboard",
            "aura.local.",
            port,
            &[("path", "/")],
        )?;
        self.web_registered = Some(info.get_fullname().to_string());
        self.register(info)
    }

    /// Re-announces our records if this PC's LAN addresses changed (Wi-Fi switch,
    /// sleep/wake, a new IPv6 prefix from the provider). Cheap; call it periodically.
    pub fn refresh(&mut self) -> Result<()> {
        let addrs = lan_addresses();
        if addrs == self.addrs {
            return Ok(());
        }
        tracing::info!("LAN addresses changed; re-announcing over mDNS");
        self.addrs = addrs;
        if let Some(advert) = self.advert.clone() {
            self.advertise(&advert)?;
        }
        self.advertise_web(self.web_port)
    }

    /// (Re-)registers our service, e.g. after joining a group.
    pub fn advertise(&mut self, advert: &Advert) -> Result<()> {
        self.advert = Some(advert.clone());
        if let Some(old) = self.registered.take() {
            let _ = self.daemon.unregister(&old);
        }
        let did = advert.device_id.to_string();
        let (gid, gname) = match &advert.group {
            Some((id, name)) => (id.to_string(), name.clone()),
            None => ("none".to_string(), String::new()),
        };
        let v = PROTOCOL_VERSION.to_string();
        let props = [
            ("v", v.as_str()),
            ("did", did.as_str()),
            ("name", advert.name.as_str()),
            ("gid", gid.as_str()),
            ("gname", gname.as_str()),
        ];
        let host = format!("aura-{}.local.", advert.device_id.short());
        let info = self.service_info(SERVICE_TYPE, &did, &host, advert.port, &props)?;
        self.registered = Some(info.get_fullname().to_string());
        self.register(info)
    }

    /// A service record pointing at our LAN addresses. Only if we have none (no
    /// network route at all) does the library pick addresses itself.
    fn service_info(
        &self,
        ty: &str,
        instance: &str,
        host: &str,
        port: u16,
        props: &[(&str, &str)],
    ) -> Result<ServiceInfo> {
        if self.addrs.is_empty() {
            return Ok(ServiceInfo::new(ty, instance, host, "", port, props)?.enable_addr_auto());
        }
        Ok(ServiceInfo::new(
            ty,
            instance,
            host,
            &self.addrs[..],
            port,
            props,
        )?)
    }

    /// Registers without the RFC 6762 conflict probe. After a sleep/wake or when the
    /// LAN's addresses change, the probe sees our own earlier announcement (with the
    /// old addresses), takes it for another device and renames us, e.g. `aura.local`
    /// to `aura-24216.local`, which phones then can't find. Our names are ours: the
    /// device host name embeds the device id, and only the elected PC claims
    /// `aura.local`.
    fn register(&self, mut info: ServiceInfo) -> Result<()> {
        info.set_requires_probe(false);
        self.daemon.register(info)?;
        Ok(())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        for name in [self.registered.take(), self.web_registered.take()]
            .into_iter()
            .flatten()
        {
            let _ = self.daemon.unregister(&name);
        }
        let _ = self.daemon.shutdown();
    }
}

/// The addresses phones and PCs can actually reach us on: those of the interfaces
/// carrying this machine's default IPv4 and IPv6 routes (Wi-Fi or Ethernet),
/// without link-local ones.
///
/// Letting the mDNS library collect addresses itself announced dead ones too:
/// loopback's `fe80::1` and other interfaces' `169.254.x.x`. iPhones prefer
/// link-local addresses, picked `fe80::1` and timed out.
pub fn lan_addresses() -> Vec<IpAddr> {
    let route_v4 = route_source(SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 9)));
    let route_v6 = route_source(SocketAddr::from((
        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
        9,
    )));
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    let lan: BTreeSet<&str> = ifaces
        .iter()
        .filter(|i| Some(i.ip()) == route_v4 || Some(i.ip()) == route_v6)
        .map(|i| i.name.as_str())
        .collect();
    let mut addrs: Vec<IpAddr> = ifaces
        .iter()
        .filter(|i| lan.contains(i.name.as_str()) && i.is_oper_up() && !i.is_loopback())
        .map(|i| i.ip())
        .filter(|ip| !is_link_local(ip))
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs
}

/// The local address the OS would use to reach `target` (no packet is sent).
fn route_source(target: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = match target {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(target).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

fn parse(svc: &mdns_sd::ResolvedService) -> Option<Discovered> {
    if svc.get_property_val_str("v")? != PROTOCOL_VERSION.to_string() {
        return None;
    }
    let device_id: DeviceId = svc.get_property_val_str("did")?.parse().ok()?;
    let name = svc
        .get_property_val_str("name")
        .unwrap_or_default()
        .to_string();
    let group = match svc.get_property_val_str("gid") {
        Some(g) if g != "none" => {
            let gid: GroupId = g.parse().ok()?;
            Some((
                gid,
                svc.get_property_val_str("gname")
                    .unwrap_or_default()
                    .to_string(),
            ))
        }
        _ => None,
    };
    // Prefer IPv4: link-local IPv6 needs scope ids that do not survive into SocketAddr well.
    let mut addrs: Vec<SocketAddr> = svc
        .get_addresses_v4()
        .into_iter()
        .map(|ip| SocketAddr::new(ip.into(), svc.port))
        .collect();
    // Loopback is only useful when both nodes run on the same PC (development).
    if addrs.iter().any(|a| !a.ip().is_loopback()) {
        addrs.retain(|a| !a.ip().is_loopback());
    }
    addrs.sort();
    if addrs.is_empty() {
        return None;
    }
    Some(Discovered {
        device_id,
        name,
        group,
        addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_addresses_are_reachable_kinds_only() {
        // Whatever network the test runs on: never loopback or link-local.
        for ip in lan_addresses() {
            assert!(!ip.is_loopback(), "{ip}");
            assert!(!is_link_local(&ip), "{ip}");
        }
    }

    #[test]
    fn link_local_detection() {
        assert!(is_link_local(&"fe80::1".parse().unwrap()));
        assert!(is_link_local(&"fe80::c6:a1ff:b258:6a0".parse().unwrap()));
        assert!(is_link_local(&"169.254.150.183".parse().unwrap()));
        assert!(!is_link_local(&"2804:235c:111:6400::ccaa".parse().unwrap()));
        assert!(!is_link_local(&"192.168.1.2".parse().unwrap()));
    }
}
