//! The iroh endpoint: built with `presets::Minimal` (no n0 relays, no address lookup),
//! relays only from jaunt's settings, the OS resolver (no public DNS fallback), and an
//! after-handshake hook that admits only current roster members.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::endpoint::{
    AfterHandshakeOutcome, Connection, EndpointHooks, NetReportConfig, PortmapperConfig, QuicTransportConfig, presets,
};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey, TransportAddr};

use crate::proto::{ALPN, PAIR_ALPN, PathKind};
use crate::roster::MachineId;

pub const KEEP_ALIVE: Duration = Duration::from_secs(5);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// QUIC application close codes.
pub const CLOSE_NORMAL: u32 = 0;
pub const CLOSE_NOT_MEMBER: u32 = 403;

/// The live set of member ids, shared with the handshake hook.
#[derive(Debug, Default)]
pub struct Members(RwLock<Arc<HashSet<MachineId>>>);

impl Members {
    pub fn set(&self, ids: HashSet<MachineId>) {
        if let Ok(mut g) = self.0.write() {
            *g = Arc::new(ids);
        }
    }
    pub fn contains(&self, id: &MachineId) -> bool {
        self.0.read().map(|g| g.contains(id)).unwrap_or(false)
    }
    pub fn snapshot(&self) -> Arc<HashSet<MachineId>> {
        self.0.read().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Rejects every remote that is not a current member, before any stream is accepted.
/// Runs for incoming and outgoing connections; compares the raw 32 key bytes.
#[derive(Debug)]
struct Allowlist(Arc<Members>);

impl EndpointHooks for Allowlist {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let id = MachineId::from_endpoint(&conn.remote_id());
        if self.0.contains(&id) {
            AfterHandshakeOutcome::accept()
        } else {
            tracing::info!("refused connection from {}: not in the roster", id.short());
            AfterHandshakeOutcome::Reject { error_code: CLOSE_NOT_MEMBER.into(), reason: b"not a member of this mesh".to_vec() }
        }
    }
}

/// DNS via the OS (getaddrinfo): honours /etc/hosts and the system resolver, and never
/// falls back to iroh's built-in public resolvers.
#[derive(Debug, Clone)]
struct OsResolver;

type DnsFut<T> = n0_future::boxed::BoxFuture<Result<iroh::dns::BoxIter<T>, iroh::dns::DnsError>>;

async fn os_lookup(host: String) -> Result<Vec<IpAddr>, iroh::dns::DnsError> {
    let addrs = tokio::net::lookup_host((host.as_str(), 0))
        .await
        .map_err(|e| iroh::dns::DnsError::from(n0_error::AnyError::from_std(e)))?;
    Ok(addrs.map(|a| a.ip()).collect())
}

impl iroh::dns::Resolver for OsResolver {
    fn lookup_ipv4(&self, host: String) -> DnsFut<std::net::Ipv4Addr> {
        Box::pin(async move {
            let v: Vec<_> = os_lookup(host).await?.into_iter().filter_map(|a| match a { IpAddr::V4(v) => Some(v), _ => None }).collect();
            Ok(Box::new(v.into_iter()) as iroh::dns::BoxIter<_>)
        })
    }
    fn lookup_ipv6(&self, host: String) -> DnsFut<std::net::Ipv6Addr> {
        Box::pin(async move {
            let v: Vec<_> = os_lookup(host).await?.into_iter().filter_map(|a| match a { IpAddr::V6(v) => Some(v), _ => None }).collect();
            Ok(Box::new(v.into_iter()) as iroh::dns::BoxIter<_>)
        })
    }
    fn lookup_txt(&self, _host: String) -> DnsFut<iroh::dns::TxtRecordData> {
        Box::pin(async move { Err(iroh::dns::DnsError::from(n0_error::AnyError::from("jaunt does no DNS address lookup"))) })
    }
    fn clear_cache(&self) {}
    fn reset(&self) -> Box<dyn iroh::dns::Resolver> {
        Box::new(self.clone())
    }
}

pub struct EndpointOptions<'a> {
    pub secret: SecretKey,
    pub relays: &'a [RelayUrl],
    pub relay_ca: Option<&'a Path>,
    pub portmapper: bool,
    pub port: Option<u16>,
    pub members: Arc<Members>,
}

pub fn parse_relays(list: &[String]) -> Result<Vec<RelayUrl>> {
    list.iter()
        .map(|s| s.parse::<RelayUrl>().with_context(|| format!("'{s}' is not a valid relay URL")))
        .collect()
}

/// Builder settings shared by the machine endpoint and the pairing endpoints.
fn base_builder(relays: &[RelayUrl], relay_ca: Option<&Path>, portmapper: bool) -> Result<iroh::endpoint::Builder> {
    // Never `RelayMode::Disabled`, even with no relay: that leaves the endpoint without a
    // relay transport, and a relay added later (`jaunt relay set`, or a relay list that
    // arrives by roster sync) would never be used. An empty custom map contacts nobody.
    let relay_mode = RelayMode::custom(relays.iter().cloned());
    let transport = QuicTransportConfig::builder()
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
        .build();
    // No captive-portal probe: it would go to port 80 of the relay host, which is not
    // the configured relay URL. HTTPS latency probes go to the relay URL itself.
    let mut net_report = NetReportConfig::minimal();
    net_report.https_probes = true;
    let mut b = Endpoint::builder(presets::Minimal);
    if let Some(ca) = relay_ca {
        use rustls_pki_types::{CertificateDer, pem::PemObject};
        let certs = CertificateDer::pem_file_iter(ca)
            .with_context(|| format!("cannot read relay CA {}", ca.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("cannot parse relay CA {}", ca.display()))?;
        b = b.ca_tls_config(iroh::tls::CaTlsConfig::custom_roots(certs));
    }
    Ok(b.dns_resolver(iroh::dns::DnsResolver::custom(OsResolver))
        .relay_mode(relay_mode)
        .transport_config(transport)
        .portmapper_config(if portmapper { PortmapperConfig::default() } else { PortmapperConfig::Disabled })
        .net_report_config(net_report))
}

pub async fn build_endpoint(o: EndpointOptions<'_>) -> Result<Endpoint> {
    let mut b = base_builder(o.relays, o.relay_ca, o.portmapper)?;
    if let Some(port) = o.port {
        b = b.bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))?;
    }
    let ep = b
        .secret_key(o.secret)
        .alpns(vec![ALPN.to_vec()])
        .hooks(Allowlist(o.members))
        .bind()
        .await
        .context("cannot start the network endpoint")?;
    Ok(ep)
}

/// A short-lived endpoint for pairing (design §6). It speaks only `jaunt-pair/1`, accepts
/// any remote (the PAKE is the authentication), and never asks the gateway for a mapping.
/// `accept` = it is the inviter's rendezvous endpoint; the joiner's only dials.
pub async fn build_pair_endpoint(secret: SecretKey, relays: &[RelayUrl], relay_ca: Option<&Path>, accept: bool) -> Result<Endpoint> {
    let alpns = if accept { vec![PAIR_ALPN.to_vec()] } else { Vec::new() };
    base_builder(relays, relay_ca, false)?
        .secret_key(secret)
        .alpns(alpns)
        .bind()
        .await
        .context("cannot start the pairing endpoint")
}

/// What to dial for a peer: its id, every relay (no address lookup exists), and the direct
/// addresses it was last reached on (`hints`). The hints only say where to try: the peer is
/// still authenticated by its key, so a stale one costs a failed attempt and nothing more.
pub fn dial_addr(id: MachineId, relays: &[RelayUrl], hints: &[SocketAddr]) -> Result<EndpointAddr> {
    let mut addr = EndpointAddr::new(id.to_endpoint()?);
    for r in relays {
        addr = addr.with_relay_url(r.clone());
    }
    for h in hints {
        addr = addr.with_ip_addr(*h);
    }
    Ok(addr)
}

/// The selected path of a connection.
#[derive(Debug, Clone, PartialEq)]
pub struct PathInfo {
    pub kind: PathKind,
    pub rtt: Duration,
    pub remote: String,
    pub addr_kind: &'static str,
}

pub fn selected_path(conn: &Connection) -> Option<PathInfo> {
    let paths = conn.paths();
    let p = paths.iter().find(|p| p.is_selected())?;
    let (kind, remote, addr_kind) = describe_addr(p.remote_addr());
    Some(PathInfo { kind, rtt: p.rtt(), remote, addr_kind })
}

pub fn describe_addr(a: &TransportAddr) -> (PathKind, String, &'static str) {
    match a {
        TransportAddr::Relay(u) => (PathKind::Relay, u.to_string(), "relay"),
        TransportAddr::Ip(sa) => (PathKind::Direct, sa.to_string(), ip_kind(sa.ip())),
        _ => (PathKind::Unknown, format!("{a:?}"), "other"),
    }
}

pub fn ip_kind(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(v) if v.is_loopback() => "loopback",
        IpAddr::V4(v) if v.is_private() || v.is_link_local() => "lan",
        IpAddr::V4(v) if v.octets()[0] == 100 && (v.octets()[1] & 0xc0) == 64 => "cgnat",
        IpAddr::V4(_) => "public",
        IpAddr::V6(v) if v.is_loopback() => "loopback",
        IpAddr::V6(v) if (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80 => "lan",
        IpAddr::V6(_) => "public",
    }
}

/// Home relay connected?
pub fn relay_connected(ep: &Endpoint) -> bool {
    use n0_watcher::Watcher;
    ep.home_relay_status().get().iter().any(|s| s.is_connected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_kinds() {
        assert_eq!(ip_kind("10.0.0.1".parse().unwrap()), "lan");
        assert_eq!(ip_kind("192.168.1.4".parse().unwrap()), "lan");
        assert_eq!(ip_kind("127.0.0.1".parse().unwrap()), "loopback");
        assert_eq!(ip_kind("100.72.1.1".parse().unwrap()), "cgnat");
        assert_eq!(ip_kind("198.51.100.11".parse().unwrap()), "public");
        assert_eq!(ip_kind("fe80::1".parse().unwrap()), "lan");
        assert_eq!(ip_kind("2001:db8::1".parse().unwrap()), "public");
    }

    #[test]
    fn members_set() {
        let m = Members::default();
        assert!(!m.contains(&MachineId([1; 32])));
        m.set([MachineId([1; 32])].into_iter().collect());
        assert!(m.contains(&MachineId([1; 32])));
    }

    #[test]
    fn relay_parsing() {
        assert!(parse_relays(&["https://relay.example.org".into()]).is_ok());
        assert!(parse_relays(&["nope".into()]).is_err());
    }
}
