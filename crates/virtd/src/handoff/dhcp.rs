//! Minimal DHCPv4 responder that hands the pod's own IP configuration to the
//! guest. It answers exactly one client (the guest NIC, identified by the pod
//! MAC) with exactly one address (the pod IP). It is not a general DHCP
//! server: no pools, no leases database.
//!
//! Frames are received on a packet socket bound to the TAP inside the pod
//! network namespace and replies are sent as broadcast IPv4 UDP frames on
//! the same socket.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, OwnedFd};

use dhcproto::v4::{DhcpOption, Message, MessageType, Opcode, OptionCode};
use dhcproto::{Decodable, Decoder, Encodable, Encoder};
use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use baryon_cloud_hypervisor::launch::MacAddr;

use super::network::PodIpv4;
#[cfg(test)]
use super::network::PodRoute;

const LEASE_SECONDS: u32 = 86_400;
const PACKET_OUTGOING: u8 = 4;

/// DNS settings read from the pod's resolv.conf.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsConfig {
    pub nameservers: Vec<Ipv4Addr>,
    pub search: Vec<String>,
}

impl DnsConfig {
    /// Parse resolv.conf(5). Only IPv4 nameservers are used.
    pub fn parse_resolv_conf(s: &str) -> Self {
        let mut out = Self::default();
        for line in s.lines() {
            let mut words = line.split_whitespace();
            match words.next() {
                Some("nameserver") => {
                    if let Some(Ok(ip)) = words.next().map(str::parse::<Ipv4Addr>) {
                        out.nameservers.push(ip);
                    }
                }
                Some("search") => out.search = words.map(str::to_owned).collect(),
                _ => {}
            }
        }
        out
    }
}

/// Everything the responder offers. Serialized as the configuration of the
/// responder process (see `crate::dhcp_helper`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseConfig {
    #[serde(with = "mac_string")]
    pub client_mac: MacAddr,
    pub ipv4: PodIpv4,
    pub mtu: u32,
    pub dns: DnsConfig,
    pub hostname: String,
}

/// Events reported to the VM supervisor. Serialized one per line on the
/// responder process's standard output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DhcpEvent {
    Acknowledged {
        ip: Ipv4Addr,
        #[serde(with = "mac_string")]
        mac: MacAddr,
    },
}

mod mac_string {
    use baryon_cloud_hypervisor::launch::MacAddr;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(mac: &MacAddr, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(mac)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<MacAddr, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serve DHCP on `socket` until `cancel` fires.
pub async fn serve(
    socket: OwnedFd,
    lease: LeaseConfig,
    events: mpsc::Sender<DhcpEvent>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let socket = AsyncFd::new(socket)?;
    let mut buf = vec![0u8; 2048];
    loop {
        let received = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            r = socket.readable() => {
                let mut guard = r?;
                match guard.try_io(|fd| recv_packet(fd.get_ref(), &mut buf)) {
                    Ok(r) => r?,
                    Err(_would_block) => continue,
                }
            }
        };
        let Some((len, pkttype)) = received else {
            continue;
        };
        if pkttype == PACKET_OUTGOING {
            continue;
        }
        let Some(request) = parse_dhcp_request(&buf[..len]) else {
            continue;
        };
        let Some((reply, acked)) = respond(&request, &lease) else {
            continue;
        };
        let frame =
            match build_ipv4_udp(server_ip(&lease.ipv4), Ipv4Addr::BROADCAST, 67, 68, &reply) {
                Some(f) => f,
                None => continue,
            };
        let ifindex = interface_index(socket.get_ref())?;
        loop {
            let mut guard = socket.writable().await?;
            match guard.try_io(|fd| send_broadcast(fd.get_ref(), ifindex, &frame)) {
                Ok(r) => {
                    r?;
                    break;
                }
                Err(_would_block) => continue,
            }
        }
        if acked {
            tracing::info!(ip = %lease.ipv4.address, mac = %lease.client_mac, "DHCP lease acknowledged");
            let _ = events
                .send(DhcpEvent::Acknowledged {
                    ip: lease.ipv4.address,
                    mac: lease.client_mac,
                })
                .await;
        }
    }
}

/// The server identifier: the pod gateway if known, otherwise the pod
/// address itself. Clients use it only to address renewals, which the
/// responder sees on the TAP regardless of destination.
fn server_ip(ipv4: &PodIpv4) -> Ipv4Addr {
    ipv4.gateway.unwrap_or(ipv4.address)
}

/// Extract a DHCP message from an IPv4 packet sent by a client to port 67.
fn parse_dhcp_request(packet: &[u8]) -> Option<Message> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(packet[0] & 0x0f) * 4;
    if ihl < 20 || packet.len() < ihl + 8 || packet[9] != 17 {
        return None;
    }
    let udp = &packet[ihl..];
    let dport = u16::from_be_bytes([udp[2], udp[3]]);
    if dport != 67 {
        return None;
    }
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    let payload = udp.get(8..udp_len.min(udp.len()))?;
    let msg = Message::decode(&mut Decoder::new(payload)).ok()?;
    (msg.opcode() == Opcode::BootRequest).then_some(msg)
}

/// Build the reply to a request. Returns the encoded DHCP message and
/// whether it is an acknowledgement.
fn respond(req: &Message, lease: &LeaseConfig) -> Option<(Vec<u8>, bool)> {
    if req.chaddr().get(..6) != Some(&lease.client_mac.0[..]) {
        return None;
    }
    let msg_type = req.opts().msg_type()?;
    let reply_type = match msg_type {
        MessageType::Discover => MessageType::Offer,
        MessageType::Request => {
            let requested = match req.opts().get(OptionCode::RequestedIpAddress) {
                Some(DhcpOption::RequestedIpAddress(ip)) => Some(*ip),
                _ => None,
            };
            let ciaddr = req.ciaddr();
            let wants = requested.or((!ciaddr.is_unspecified()).then_some(ciaddr));
            match wants {
                Some(ip) if ip != lease.ipv4.address => MessageType::Nak,
                _ => MessageType::Ack,
            }
        }
        // The guest only ever gets the pod address.
        _ => return None,
    };

    let mut reply = Message::default();
    reply
        .set_opcode(Opcode::BootReply)
        .set_xid(req.xid())
        .set_flags(req.flags())
        .set_chaddr(&lease.client_mac.0)
        .set_giaddr(req.giaddr());
    let opts = reply.opts_mut();
    opts.insert(DhcpOption::MessageType(reply_type));
    opts.insert(DhcpOption::ServerIdentifier(server_ip(&lease.ipv4)));
    if reply_type != MessageType::Nak {
        reply.set_yiaddr(lease.ipv4.address);
        let opts = reply.opts_mut();
        opts.insert(DhcpOption::AddressLeaseTime(LEASE_SECONDS));
        opts.insert(DhcpOption::Renewal(LEASE_SECONDS / 2));
        opts.insert(DhcpOption::Rebinding(LEASE_SECONDS / 8 * 7));
        // The guest gets a /32 and the pod's own routes as classless static
        // routes (RFC 3442), so it reaches every destination exactly as the
        // pod would. CNIs differ in whether same-subnet peers are on-link
        // (bridge) or behind the gateway (routed); only the pod's routing
        // table says which.
        let routes = classless_routes(&lease.ipv4);
        if routes.is_empty() {
            opts.insert(DhcpOption::SubnetMask(prefix_to_mask(
                lease.ipv4.prefix_len,
            )));
        } else {
            opts.insert(DhcpOption::SubnetMask(Ipv4Addr::BROADCAST));
            opts.insert(DhcpOption::ClasslessStaticRoute(routes));
        }
        // Clients without option 121 support fall back to the router option;
        // clients with it must ignore option 3.
        if let Some(gw) = lease.ipv4.gateway {
            opts.insert(DhcpOption::Router(vec![gw]));
        }
        if !lease.dns.nameservers.is_empty() {
            opts.insert(DhcpOption::DomainNameServer(lease.dns.nameservers.clone()));
        }
        let search: Vec<dhcproto::Name> = lease
            .dns
            .search
            .iter()
            .filter_map(|d| d.parse().ok())
            .collect();
        if !search.is_empty() {
            opts.insert(DhcpOption::DomainSearch(search));
        }
        if let Ok(mtu) = u16::try_from(lease.mtu) {
            opts.insert(DhcpOption::InterfaceMtu(mtu));
        }
        if !lease.hostname.is_empty() {
            opts.insert(DhcpOption::Hostname(lease.hostname.clone()));
        }
    }

    let mut bytes = Vec::new();
    reply.encode(&mut Encoder::new(&mut bytes)).ok()?;
    Some((bytes, reply_type == MessageType::Ack))
}

fn prefix_to_mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    };
    Ipv4Addr::from(bits)
}

/// Routes for option 121, on-link routes first so that every gateway is
/// reachable before routes through it are installed.
fn classless_routes(ipv4: &PodIpv4) -> Vec<(ipnet::Ipv4Net, Ipv4Addr)> {
    let mut on_link = Vec::new();
    let mut via = Vec::new();
    for r in &ipv4.routes {
        let Ok(net) = ipnet::Ipv4Net::new(r.destination, r.prefix_len) else {
            continue;
        };
        match r.gateway {
            None => on_link.push((net.trunc(), Ipv4Addr::UNSPECIFIED)),
            Some(gw) => via.push((net.trunc(), gw)),
        }
    }
    // Every gateway must be on-link. Add a host route when the pod's table
    // does not already cover it.
    for (_, gw) in &via {
        if !on_link.iter().any(|(n, _)| n.contains(gw))
            && let Ok(host) = ipnet::Ipv4Net::new(*gw, 32)
        {
            on_link.push((host, Ipv4Addr::UNSPECIFIED));
        }
    }
    on_link.extend(via);
    on_link
}

/// IPv4 header plus UDP header plus payload, with checksums.
fn build_ipv4_udp(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let udp_len = u16::try_from(8 + payload.len()).ok()?;
    let total = u16::try_from(20 + 8 + payload.len()).ok()?;
    let mut p = Vec::with_capacity(usize::from(total));
    p.extend_from_slice(&[0x45, 0x00]);
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]); // id, flags, fragment offset
    p.extend_from_slice(&[64, 17, 0, 0]); // ttl, protocol udp, checksum
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let ip_csum = checksum(&p[..20], 0);
    p[10..12].copy_from_slice(&ip_csum.to_be_bytes());

    p.extend_from_slice(&sport.to_be_bytes());
    p.extend_from_slice(&dport.to_be_bytes());
    p.extend_from_slice(&udp_len.to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(payload);

    let mut pseudo = 0u32;
    for chunk in src.octets().chunks(2).chain(dst.octets().chunks(2)) {
        pseudo += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    pseudo += 17 + u32::from(udp_len);
    let mut udp_csum = checksum(&p[20..], pseudo);
    if udp_csum == 0 {
        udp_csum = 0xffff;
    }
    p[26..28].copy_from_slice(&udp_csum.to_be_bytes());
    Some(p)
}

fn checksum(data: &[u8], initial: u32) -> u16 {
    let mut sum = initial;
    let (pairs, rest) = data.as_chunks::<2>();
    for c in pairs {
        sum += u32::from(u16::from_be_bytes(*c));
    }
    if let [last] = rest {
        sum += u32::from(*last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[allow(unsafe_code)]
fn recv_packet(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<Option<(usize, u8)>> {
    // SAFETY: all-zero bytes are a valid sockaddr_ll.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    // SAFETY: `buf` is a valid writable buffer of the given length, `sll` and
    // `len` are valid out-parameters sized for sockaddr_ll, and `fd` is an
    // open socket.
    let n = unsafe {
        libc::recvfrom(
            fd.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            (&raw mut sll).cast(),
            &raw mut len,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some((usize::try_from(n).unwrap_or(0), sll.sll_pkttype)))
}

#[allow(unsafe_code)]
fn interface_index(fd: &OwnedFd) -> io::Result<i32> {
    // SAFETY: all-zero bytes are a valid sockaddr_ll.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    // SAFETY: `sll` and `len` are valid out-parameters sized for sockaddr_ll.
    let rc = unsafe { libc::getsockname(fd.as_raw_fd(), (&raw mut sll).cast(), &raw mut len) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sll.sll_ifindex)
}

#[allow(unsafe_code)]
fn send_broadcast(fd: &OwnedFd, ifindex: i32, frame: &[u8]) -> io::Result<()> {
    // SAFETY: all-zero bytes are a valid sockaddr_ll.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as libc::c_ushort;
    sll.sll_protocol = 0x0800u16.to_be();
    sll.sll_ifindex = ifindex;
    sll.sll_halen = 6;
    sll.sll_addr[..6].copy_from_slice(&[0xff; 6]);
    // SAFETY: `frame` is a valid buffer, `sll` is a valid initialized
    // sockaddr_ll and its size is passed. `fd` is an open packet socket.
    let n = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            frame.as_ptr().cast(),
            frame.len(),
            0,
            (&raw const sll).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(d: [u8; 4], len: u8, gw: Option<[u8; 4]>) -> PodRoute {
        PodRoute {
            destination: Ipv4Addr::from(d),
            prefix_len: len,
            gateway: gw.map(Ipv4Addr::from),
        }
    }

    fn lease(prefix: u8, gw: Ipv4Addr) -> LeaseConfig {
        lease_with_routes(prefix, gw, Vec::new())
    }

    fn lease_with_routes(prefix: u8, gw: Ipv4Addr, routes: Vec<PodRoute>) -> LeaseConfig {
        LeaseConfig {
            client_mac: "02:42:0a:f4:01:05".parse().unwrap(),
            ipv4: PodIpv4 {
                address: Ipv4Addr::new(10, 244, 1, 5),
                prefix_len: prefix,
                gateway: Some(gw),
                routes,
            },
            mtu: 1450,
            dns: DnsConfig {
                nameservers: vec![Ipv4Addr::new(10, 96, 0, 10)],
                search: vec![
                    "default.svc.cluster.local".into(),
                    "svc.cluster.local".into(),
                ],
            },
            hostname: "vm1".into(),
        }
    }

    fn request(kind: MessageType, mac: &[u8], requested: Option<Ipv4Addr>) -> Message {
        let mut m = Message::default();
        m.set_opcode(Opcode::BootRequest)
            .set_xid(0xdead_beef)
            .set_chaddr(mac);
        m.opts_mut().insert(DhcpOption::MessageType(kind));
        if let Some(ip) = requested {
            m.opts_mut().insert(DhcpOption::RequestedIpAddress(ip));
        }
        m
    }

    fn decode(bytes: &[u8]) -> Message {
        Message::decode(&mut Decoder::new(bytes)).unwrap()
    }

    #[test]
    fn offers_pod_address_in_prefix() {
        let l = lease(24, Ipv4Addr::new(10, 244, 1, 1));
        let (bytes, acked) =
            respond(&request(MessageType::Discover, &l.client_mac.0, None), &l).unwrap();
        assert!(!acked);
        let r = decode(&bytes);
        assert_eq!(r.opts().msg_type(), Some(MessageType::Offer));
        assert_eq!(r.yiaddr(), Ipv4Addr::new(10, 244, 1, 5));
        assert_eq!(r.xid(), 0xdead_beef);
        assert!(matches!(r.opts().get(OptionCode::SubnetMask),
            Some(DhcpOption::SubnetMask(m)) if *m == Ipv4Addr::new(255, 255, 255, 0)));
        assert!(matches!(r.opts().get(OptionCode::Router),
            Some(DhcpOption::Router(r)) if r == &vec![Ipv4Addr::new(10, 244, 1, 1)]));
        assert!(matches!(
            r.opts().get(OptionCode::InterfaceMtu),
            Some(DhcpOption::InterfaceMtu(1450))
        ));
        assert!(
            matches!(r.opts().get(OptionCode::DomainSearch), Some(DhcpOption::DomainSearch(d)) if d.len() == 2)
        );
    }

    fn classless(l: &LeaseConfig) -> (Ipv4Addr, Vec<(String, Ipv4Addr)>) {
        let (bytes, _) =
            respond(&request(MessageType::Discover, &l.client_mac.0, None), l).unwrap();
        let r = decode(&bytes);
        let Some(DhcpOption::SubnetMask(mask)) = r.opts().get(OptionCode::SubnetMask) else {
            panic!("no mask");
        };
        let Some(DhcpOption::ClasslessStaticRoute(routes)) =
            r.opts().get(OptionCode::ClasslessStaticRoute)
        else {
            panic!("no classless routes");
        };
        (
            *mask,
            routes.iter().map(|(n, g)| (n.to_string(), *g)).collect(),
        )
    }

    #[test]
    fn routed_cni_keeps_same_subnet_behind_gateway() {
        // kindnet: 10.244.1.5/24 but the whole /24 is routed via the gateway.
        let l = lease_with_routes(
            24,
            Ipv4Addr::new(10, 244, 1, 1),
            vec![
                route([0, 0, 0, 0], 0, Some([10, 244, 1, 1])),
                route([10, 244, 1, 0], 24, Some([10, 244, 1, 1])),
                route([10, 244, 1, 1], 32, None),
            ],
        );
        let (mask, routes) = classless(&l);
        assert_eq!(mask, Ipv4Addr::BROADCAST);
        let gw = Ipv4Addr::new(10, 244, 1, 1);
        assert_eq!(
            routes,
            vec![
                ("10.244.1.1/32".into(), Ipv4Addr::UNSPECIFIED),
                ("0.0.0.0/0".into(), gw),
                ("10.244.1.0/24".into(), gw),
            ]
        );
    }

    #[test]
    fn bridged_cni_keeps_subnet_on_link() {
        // flannel: connected /24 on a bridge, cluster CIDR via the bridge IP.
        let l = lease_with_routes(
            24,
            Ipv4Addr::new(10, 244, 1, 1),
            vec![
                route([10, 244, 1, 0], 24, None),
                route([10, 244, 0, 0], 16, Some([10, 244, 1, 1])),
                route([0, 0, 0, 0], 0, Some([10, 244, 1, 1])),
            ],
        );
        let (_, routes) = classless(&l);
        assert_eq!(routes[0], ("10.244.1.0/24".into(), Ipv4Addr::UNSPECIFIED));
        // The gateway is inside the on-link subnet, so no extra host route.
        assert_eq!(routes.len(), 3);
    }

    #[test]
    fn adds_host_route_for_off_link_gateway() {
        // Calico style: /32 address, gateway 169.254.1.1 reached on-link.
        let l = lease_with_routes(
            32,
            Ipv4Addr::new(169, 254, 1, 1),
            vec![route([0, 0, 0, 0], 0, Some([169, 254, 1, 1]))],
        );
        let (_, routes) = classless(&l);
        assert_eq!(
            routes,
            vec![
                ("169.254.1.1/32".into(), Ipv4Addr::UNSPECIFIED),
                ("0.0.0.0/0".into(), Ipv4Addr::new(169, 254, 1, 1)),
            ]
        );
    }

    #[test]
    fn acks_matching_request_and_naks_others() {
        let l = lease(24, Ipv4Addr::new(10, 244, 1, 1));
        let (bytes, acked) = respond(
            &request(
                MessageType::Request,
                &l.client_mac.0,
                Some(Ipv4Addr::new(10, 244, 1, 5)),
            ),
            &l,
        )
        .unwrap();
        assert!(acked);
        assert_eq!(decode(&bytes).opts().msg_type(), Some(MessageType::Ack));

        let (bytes, acked) = respond(
            &request(
                MessageType::Request,
                &l.client_mac.0,
                Some(Ipv4Addr::new(10, 0, 0, 9)),
            ),
            &l,
        )
        .unwrap();
        assert!(!acked);
        assert_eq!(decode(&bytes).opts().msg_type(), Some(MessageType::Nak));
    }

    #[test]
    fn ignores_other_clients() {
        let l = lease(24, Ipv4Addr::new(10, 244, 1, 1));
        assert!(
            respond(
                &request(MessageType::Discover, &[2, 0, 0, 0, 0, 1], None),
                &l
            )
            .is_none()
        );
    }

    #[test]
    fn frame_round_trips_through_parser() {
        let l = lease(24, Ipv4Addr::new(10, 244, 1, 1));
        let req = request(MessageType::Discover, &l.client_mac.0, None);
        let mut payload = Vec::new();
        req.encode(&mut Encoder::new(&mut payload)).unwrap();
        let frame =
            build_ipv4_udp(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &payload).unwrap();
        // IPv4 header checksum verifies to zero.
        assert_eq!(checksum(&frame[..20], 0), 0);
        let parsed = parse_dhcp_request(&frame).unwrap();
        assert_eq!(parsed.xid(), 0xdead_beef);
        // Replies to port 68 are not parsed as requests.
        let reply =
            build_ipv4_udp(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 67, 68, &payload).unwrap();
        assert!(parse_dhcp_request(&reply).is_none());
    }

    #[test]
    fn parses_resolv_conf() {
        let d = DnsConfig::parse_resolv_conf(
            "search default.svc.cluster.local svc.cluster.local cluster.local\nnameserver 10.96.0.10\nnameserver fd00::10\noptions ndots:5\n",
        );
        assert_eq!(d.nameservers, vec![Ipv4Addr::new(10, 96, 0, 10)]);
        assert_eq!(d.search.len(), 3);
    }

    #[test]
    fn masks() {
        assert_eq!(prefix_to_mask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(prefix_to_mask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(prefix_to_mask(32), Ipv4Addr::new(255, 255, 255, 255));
    }
}
