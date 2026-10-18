//! mDNS / DNS-SD: every node advertises `_aura._tcp.local.` with its device id,
//! name and group, and browses for the others.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Result;
use aura_core::{DeviceId, GroupId, PROTOCOL_VERSION};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
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
}

impl Discovery {
    /// Starts advertising (if `advert` is given) and browsing. Events about other
    /// devices arrive on the returned channel.
    pub fn start(advert: Option<&Advert>) -> Result<(Self, mpsc::Receiver<DiscoveryEvent>)> {
        let daemon = ServiceDaemon::new()?;
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
        };
        if let Some(advert) = advert {
            d.advertise(advert)?;
        }
        Ok((d, rx))
    }

    /// Claims (`Some(port)`) or releases (`None`) the `aura.local` host name for the
    /// phone page. Only the fofoqueiro holds it, so the name follows the role.
    pub fn advertise_web(&mut self, port: Option<u16>) -> Result<()> {
        if let Some(old) = self.web_registered.take() {
            let _ = self.daemon.unregister(&old);
        }
        let Some(port) = port else {
            return Ok(());
        };
        let info = ServiceInfo::new(
            "_https._tcp.local.",
            "Aura Clipboard",
            "aura.local.",
            "",
            port,
            &[("path", "/")][..],
        )?
        .enable_addr_auto();
        self.web_registered = Some(info.get_fullname().to_string());
        self.daemon.register(info)?;
        Ok(())
    }

    /// (Re-)registers our service, e.g. after joining a group.
    pub fn advertise(&mut self, advert: &Advert) -> Result<()> {
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
        let info = ServiceInfo::new(SERVICE_TYPE, &did, &host, "", advert.port, &props[..])?
            .enable_addr_auto();
        self.registered = Some(info.get_fullname().to_string());
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
