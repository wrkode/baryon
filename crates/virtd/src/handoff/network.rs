//! Network handoff: connect a guest NIC to the Anchor Pod network.
//!
//! All work happens inside the pod network namespace on a dedicated OS
//! thread that enters the namespace through the Anchor pidfd. The thread is
//! never reused, so no other code ever runs in the pod namespace by
//! accident. Sockets and TAP descriptors created there stay bound to the pod
//! namespace after the thread exits. See ADR 0007.
//!
//! Layout inside the pod network namespace while a VM runs:
//!
//! ```text
//!   pod interface (eth0)                 TAP (bvm0)
//!   ingress: u32 match all -> mirred redirect ---> egress to guest
//!   egress <--- mirred redirect <- u32 match all   ingress (prio 2)
//!                    IPv4 UDP to :67 -> gact ok (prio 1): not redirected,
//!                    delivered to the pod network stack, where the DHCP
//!                    responder's packet socket receives it
//! ```
//!
//! Kernel receive order matters here (net/core/dev.c,
//! `__netif_receive_skb_core`): `ETH_P_ALL` taps run first, then tc ingress,
//! then protocol handlers including device-bound protocol-specific packet
//! sockets. The responder's `ETH_P_IP` socket bound to the TAP therefore only
//! sees frames that tc lets through, which is why DHCP is passed rather than
//! dropped.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use futures::TryStreamExt;
use netlink_packet_core::DefaultNla;
use netlink_packet_route::AddressFamily;
use netlink_packet_route::address::{AddressAttribute, AddressMessage};
use netlink_packet_route::link::{LinkAttribute, LinkMessage};
use netlink_packet_route::route::{RouteAddress, RouteAttribute, RouteMessage};
use netlink_packet_route::tc::{
    TcAction, TcActionAttribute, TcActionOption, TcFilterU32Option, TcHandle, TcU32Key,
    TcU32Selector, TcU32SelectorFlags,
};
use rustix::fs::{Mode, OFlags};

use baryon_cloud_hypervisor::launch::MacAddr;

/// Name of the TAP created in every pod network namespace. The namespace is
/// private to the pod, so a fixed name cannot collide.
pub const TAP_NAME: &str = "bvm0";

const ETH_P_ALL: u16 = 0x0003;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_ARP: u16 = 0x0806;
/// `TC_H_INGRESS` parent for filters on the ingress qdisc.
const TC_H_INGRESS_PARENT: u32 = 0xffff_fff1;
const DHCP_FILTER_PRIO: u16 = 1;
const REDIRECT_FILTER_PRIO: u16 = 2;

/// Errors during network handoff.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("entering the pod network namespace: {0}")]
    Setns(#[source] io::Error),
    #[error("handoff thread failed to run: {0}")]
    Thread(String),
    #[error("creating TAP {name}: {source}")]
    Tap {
        name: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("netlink: {0}")]
    Netlink(String),
    #[error("pod interface {0:?} not found in the pod network namespace")]
    NoPodInterface(String),
    #[error("pod interface {0:?} has no MAC address")]
    NoMac(String),
    #[error("pod interface {0:?} has no IPv4 address")]
    NoIpv4(String),
    #[error("creating the DHCP socket: {0}")]
    DhcpSocket(#[source] io::Error),
}

/// IPv4 configuration of the pod interface, handed to the guest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PodIpv4 {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Option<Ipv4Addr>,
    /// IPv4 routes of the pod network namespace that use the pod interface.
    /// The guest receives exactly these, so it routes like the pod would.
    pub routes: Vec<PodRoute>,
}

/// One IPv4 route of the pod network namespace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PodRoute {
    pub destination: Ipv4Addr,
    pub prefix_len: u8,
    /// `None` for on-link routes.
    pub gateway: Option<Ipv4Addr>,
}

/// The pod interface the guest takes over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodInterface {
    pub name: String,
    pub index: u32,
    pub mac: MacAddr,
    pub mtu: u32,
    pub ipv4: PodIpv4,
}

/// Result of a successful handoff.
#[derive(Debug)]
pub struct NetworkHandoff {
    /// TAP queue descriptor to be inherited by Cloud Hypervisor. The TAP is
    /// not persistent: it is destroyed when the last descriptor closes.
    pub tap_fd: OwnedFd,
    pub pod_interface: PodInterface,
    /// Packet socket bound to the TAP for the DHCP responder.
    pub dhcp_socket: OwnedFd,
}

/// Run `f` on a fresh OS thread inside the network namespace of the process
/// behind `pidfd`.
pub fn in_pod_netns<F, T>(pidfd: BorrowedFd<'_>, f: F) -> Result<T, NetworkError>
where
    F: FnOnce() -> Result<T, NetworkError> + Send + 'static,
    T: Send + 'static,
{
    let pidfd = pidfd.try_clone_to_owned().map_err(NetworkError::Setns)?;
    let handle = std::thread::Builder::new()
        .name("baryon-netns".into())
        .spawn(move || {
            rustix::thread::move_into_thread_name_spaces(
                pidfd.as_fd(),
                rustix::thread::ThreadNameSpaceType::NETWORK,
            )
            .map_err(|e| NetworkError::Setns(e.into()))?;
            f()
        })
        .map_err(|e| NetworkError::Thread(e.to_string()))?;
    handle
        .join()
        .map_err(|_| NetworkError::Thread("handoff thread panicked".into()))?
}

/// Prepare the TAP and the L2 passthrough. Idempotent: existing Baryon tc
/// configuration on the pod interface is replaced.
pub fn prepare(
    pidfd: BorrowedFd<'_>,
    pod_interface: &str,
    guest_mac: MacAddr,
) -> Result<NetworkHandoff, NetworkError> {
    let pod_interface = pod_interface.to_owned();
    in_pod_netns(pidfd, move || {
        let tap_fd = create_tap(TAP_NAME)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| NetworkError::Thread(e.to_string()))?;
        let (pod, tap_index) = rt.block_on(configure(&pod_interface, guest_mac))?;
        let dhcp_socket = packet_socket(tap_index).map_err(NetworkError::DhcpSocket)?;
        // Peers that talked to the pod before the address change (the host
        // side of the CNI, after the disk import) still map the pod IP to the
        // old MAC and would drop guest traffic until their neighbor entry
        // expires. A gratuitous ARP updates them now.
        if let Err(e) = announce(pod.index, pod.mac, pod.ipv4.address) {
            tracing::warn!(interface = %pod.name, error = %e, "sending gratuitous ARP failed");
        }
        Ok(NetworkHandoff {
            tap_fd,
            pod_interface: pod,
            dhcp_socket,
        })
    })
}

/// Open a new DHCP packet socket on the TAP of a VM that is already running,
/// for example after a baryon-virtd restart.
pub fn reopen_dhcp_socket(
    pidfd: BorrowedFd<'_>,
    pod_interface: &str,
) -> Result<(OwnedFd, PodInterface), NetworkError> {
    let pod_interface = pod_interface.to_owned();
    in_pod_netns(pidfd, move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| NetworkError::Thread(e.to_string()))?;
        let (pod, tap_index) = rt.block_on(async {
            let (conn, handle, _) =
                rtnetlink::new_connection().map_err(|e| NetworkError::Netlink(e.to_string()))?;
            tokio::spawn(conn);
            let pod = read_pod_interface(&handle, &pod_interface).await?;
            let tap = link_by_name(&handle, TAP_NAME)
                .await?
                .ok_or_else(|| NetworkError::Netlink(format!("{TAP_NAME} not found")))?;
            Ok::<_, NetworkError>((pod, tap.header.index))
        })?;
        Ok((
            packet_socket(tap_index).map_err(NetworkError::DhcpSocket)?,
            pod,
        ))
    })
}

/// Remove the passthrough from the pod interface so the pod namespace is
/// left as CNI configured it. The TAP itself disappears with Cloud
/// Hypervisor. Missing objects are not an error.
pub fn teardown(pidfd: BorrowedFd<'_>, pod_interface: &str) -> Result<(), NetworkError> {
    let pod_interface = pod_interface.to_owned();
    in_pod_netns(pidfd, move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| NetworkError::Thread(e.to_string()))?;
        rt.block_on(async {
            let (conn, handle, _) =
                rtnetlink::new_connection().map_err(|e| NetworkError::Netlink(e.to_string()))?;
            tokio::spawn(conn);
            if let Some(link) = link_by_name(&handle, &pod_interface).await? {
                delete_ingress(&handle, link.header.index).await;
            }
            Ok(())
        })
    })
}

#[allow(unsafe_code)]
fn create_tap(name: &'static str) -> Result<OwnedFd, NetworkError> {
    const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
    let tap_err = |source: io::Error| NetworkError::Tap { name, source };

    let fd = rustix::fs::open(
        "/dev/net/tun",
        OFlags::RDWR | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| tap_err(e.into()))?;

    // SAFETY: `ifreq` is a plain C struct for which all-zero bytes are a
    // valid value.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(name.as_bytes()) {
        *dst = *src as libc::c_char;
    }
    ifr.ifr_ifru.ifru_flags =
        (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as libc::c_short;
    // SAFETY: TUNSETIFF reads a `struct ifreq` from the pointer and writes the
    // final interface name back into it. `ifr` is a valid, initialized ifreq
    // that lives for the duration of the call, and `fd` is an open
    // /dev/net/tun descriptor. The request type differs between libc
    // implementations, hence the inferred cast.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF as _, &raw mut ifr) };
    if rc < 0 {
        return Err(tap_err(io::Error::last_os_error()));
    }
    Ok(fd)
}

async fn configure(
    pod_interface: &str,
    guest_mac: MacAddr,
) -> Result<(PodInterface, u32), NetworkError> {
    let (conn, handle, _) =
        rtnetlink::new_connection().map_err(|e| NetworkError::Netlink(e.to_string()))?;
    tokio::spawn(conn);

    let mut pod = read_pod_interface(&handle, pod_interface).await?;

    // The guest keeps one MAC address for its whole life (derived from the
    // VirtualMachine UID), because guests bind network configuration to it.
    // The pod interface takes the same address so that frames the CNI sends
    // to the pod are addressed to the guest. veth supports a live address
    // change, so the pod's routes are not disturbed.
    if pod.mac != guest_mac {
        handle
            .link()
            .set(
                rtnetlink::LinkUnspec::new_with_index(pod.index)
                    .address(guest_mac.0.to_vec())
                    .build(),
            )
            .execute()
            .await
            .map_err(|e| {
                NetworkError::Netlink(format!("setting {} address to {guest_mac}: {e}", pod.name))
            })?;
        tracing::info!(interface = %pod.name, from = %pod.mac, to = %guest_mac, "pod interface takes the VM MAC address");
        pod.mac = guest_mac;
    }

    let tap = link_by_name(&handle, TAP_NAME).await?.ok_or_else(|| {
        NetworkError::Netlink(format!("{TAP_NAME} was created but is not visible"))
    })?;
    let tap_index = tap.header.index;

    handle
        .link()
        .set(
            rtnetlink::LinkUnspec::new_with_index(tap_index)
                .mtu(pod.mtu)
                .up()
                .build(),
        )
        .execute()
        .await
        .map_err(|e| NetworkError::Netlink(format!("bringing up {TAP_NAME}: {e}")))?;

    for index in [pod.index, tap_index] {
        delete_ingress(&handle, index).await;
        handle
            .qdisc()
            .add(index as i32)
            .ingress()
            .execute()
            .await
            .map_err(|e| NetworkError::Netlink(format!("adding ingress qdisc on {index}: {e}")))?;
    }

    // Guest DHCP is answered by baryon-virtd and must not reach the cluster
    // network: it is excluded from the redirect and passed up to the pod
    // network stack, where the responder's packet socket receives it.
    handle
        .traffic_filter(tap_index as i32)
        .add()
        .parent(TC_H_INGRESS_PARENT)
        .priority(DHCP_FILTER_PRIO)
        .protocol(ETH_P_IP.to_be())
        .u32(&dhcp_pass_filter())
        .map_err(|e| NetworkError::Netlink(e.to_string()))?
        .execute()
        .await
        .map_err(|e| NetworkError::Netlink(format!("adding DHCP filter: {e}")))?;

    for (from, to) in [(tap_index, pod.index), (pod.index, tap_index)] {
        handle
            .traffic_filter(from as i32)
            .add()
            .parent(TC_H_INGRESS_PARENT)
            .priority(REDIRECT_FILTER_PRIO)
            .protocol(ETH_P_ALL.to_be())
            .redirect(to)
            .map_err(|e| NetworkError::Netlink(e.to_string()))?
            .execute()
            .await
            .map_err(|e| NetworkError::Netlink(format!("adding redirect {from} -> {to}: {e}")))?;
    }

    Ok((pod, tap_index))
}

/// u32 filter matching IPv4 UDP to port 67 with a gact "ok" action, which
/// ends classification without redirecting. The UDP destination port is
/// matched at a fixed offset, which assumes an IPv4 header without options;
/// DHCP clients do not send options.
fn dhcp_pass_filter() -> Vec<TcFilterU32Option> {
    let mut sel = TcU32Selector::default();
    sel.flags = TcU32SelectorFlags::Terminal;
    sel.keys = vec![
        // Byte 9 of the IPv4 header: protocol 17 (UDP).
        u32_key(0x00ff_0000, 0x0011_0000, 8),
        // Bytes 22..24: UDP destination port 67.
        u32_key(0x0000_ffff, 67, 20),
    ];
    sel.nkeys = 2;
    vec![
        TcFilterU32Option::Selector(sel),
        TcFilterU32Option::Action(vec![gact_ok()]),
    ]
}

fn u32_key(mask: u32, val: u32, off: i32) -> TcU32Key {
    let mut k = TcU32Key::default();
    k.mask = mask;
    k.val = val;
    k.off = off;
    k
}

/// A `gact` action with verdict `TC_ACT_OK`. netlink-packet-route has no
/// typed gact support, so the `struct tc_gact` parameters are encoded here:
/// five native-endian 32-bit fields (index, capab, action, refcnt, bindcnt).
fn gact_ok() -> TcAction {
    const TCA_GACT_PARMS: u16 = 2;
    const TC_ACT_OK: i32 = 0;
    let mut parms = Vec::with_capacity(20);
    for v in [0i32, 0, TC_ACT_OK, 0, 0] {
        parms.extend_from_slice(&v.to_ne_bytes());
    }
    let mut action = TcAction::default();
    action.attributes = vec![
        TcActionAttribute::Kind("gact".to_owned()),
        TcActionAttribute::Options(vec![TcActionOption::Other(DefaultNla::new(
            TCA_GACT_PARMS,
            parms,
        ))]),
    ];
    action
}

async fn delete_ingress(handle: &rtnetlink::Handle, index: u32) {
    let mut req = handle.qdisc().del(index as i32);
    let header = &mut req.message_mut().header;
    header.parent = TcHandle::INGRESS;
    header.handle = TcHandle::from(0xffff_0000);
    // Errors mean there was no ingress qdisc.
    let _ = req.execute().await;
}

async fn link_by_name(
    handle: &rtnetlink::Handle,
    name: &str,
) -> Result<Option<LinkMessage>, NetworkError> {
    let mut links = handle.link().get().match_name(name.to_owned()).execute();
    match links.try_next().await {
        Ok(link) => Ok(link),
        // ENODEV: no such link.
        Err(rtnetlink::Error::NetlinkError(e)) if e.raw_code() == -libc::ENODEV => Ok(None),
        Err(e) => Err(NetworkError::Netlink(format!("looking up {name}: {e}"))),
    }
}

async fn read_pod_interface(
    handle: &rtnetlink::Handle,
    name: &str,
) -> Result<PodInterface, NetworkError> {
    let link = link_by_name(handle, name)
        .await?
        .ok_or_else(|| NetworkError::NoPodInterface(name.to_owned()))?;
    let index = link.header.index;

    let mut mac = None;
    let mut mtu = 1500;
    for attr in &link.attributes {
        match attr {
            LinkAttribute::Address(bytes) if bytes.len() == 6 => {
                let mut m = [0u8; 6];
                m.copy_from_slice(bytes);
                mac = Some(MacAddr(m));
            }
            LinkAttribute::Mtu(v) => mtu = *v,
            _ => {}
        }
    }
    let mac = mac.ok_or_else(|| NetworkError::NoMac(name.to_owned()))?;

    let addresses: Vec<AddressMessage> = handle
        .address()
        .get()
        .set_link_index_filter(index)
        .execute()
        .try_collect()
        .await
        .map_err(|e| NetworkError::Netlink(format!("listing addresses: {e}")))?;
    let (address, prefix_len) = addresses
        .iter()
        .filter(|a| a.header.family == AddressFamily::Inet)
        .find_map(|a| {
            a.attributes.iter().find_map(|attr| match attr {
                AddressAttribute::Address(std::net::IpAddr::V4(ip)) => {
                    Some((*ip, a.header.prefix_len))
                }
                _ => None,
            })
        })
        .ok_or_else(|| NetworkError::NoIpv4(name.to_owned()))?;

    let routes: Vec<RouteMessage> = handle
        .route()
        .get(rtnetlink::RouteMessageBuilder::<Ipv4Addr>::new().build())
        .execute()
        .try_collect()
        .await
        .map_err(|e| NetworkError::Netlink(format!("listing routes: {e}")))?;
    let pod_routes = routes_via(&routes, index);
    let gateway = pod_routes
        .iter()
        .find(|r| r.prefix_len == 0)
        .and_then(|r| r.gateway);

    Ok(PodInterface {
        name: name.to_owned(),
        index,
        mac,
        mtu,
        ipv4: PodIpv4 {
            address,
            prefix_len,
            gateway,
            routes: pod_routes,
        },
    })
}

/// IPv4 unicast routes in the main table whose output interface is `oif`.
fn routes_via(routes: &[RouteMessage], oif: u32) -> Vec<PodRoute> {
    const RT_TABLE_MAIN: u8 = 254;
    let mut out = Vec::new();
    for r in routes {
        if r.header.table != RT_TABLE_MAIN && r.header.table != 0 {
            continue;
        }
        let mut dest = Ipv4Addr::UNSPECIFIED;
        let mut gateway = None;
        let mut route_oif = None;
        let mut table_ok = r.header.table == RT_TABLE_MAIN;
        for a in &r.attributes {
            match a {
                RouteAttribute::Destination(RouteAddress::Inet(d)) => dest = *d,
                RouteAttribute::Gateway(RouteAddress::Inet(g)) => gateway = Some(*g),
                RouteAttribute::Oif(i) => route_oif = Some(*i),
                RouteAttribute::Table(t) => table_ok = *t == u32::from(RT_TABLE_MAIN),
                _ => {}
            }
        }
        if !table_ok || route_oif != Some(oif) {
            continue;
        }
        out.push(PodRoute {
            destination: dest,
            prefix_len: r.header.destination_prefix_length,
            gateway,
        });
    }
    out
}

/// Packet socket on `ifindex` receiving IPv4 frames, for the DHCP responder.
fn packet_socket(ifindex: u32) -> io::Result<OwnedFd> {
    bound_packet_socket(ifindex, ETH_P_IP)
}

/// Gratuitous ARP request announcing `ip` at `mac`: sender and target
/// protocol addresses are both `ip`, the target hardware address is zero.
fn gratuitous_arp(mac: MacAddr, ip: Ipv4Addr) -> [u8; 28] {
    let mut p = [0u8; 28];
    p[0..2].copy_from_slice(&1u16.to_be_bytes()); // Ethernet
    p[2..4].copy_from_slice(&ETH_P_IP.to_be_bytes());
    p[4] = 6;
    p[5] = 4;
    p[6..8].copy_from_slice(&1u16.to_be_bytes()); // request
    p[8..14].copy_from_slice(&mac.0);
    p[14..18].copy_from_slice(&ip.octets());
    p[24..28].copy_from_slice(&ip.octets());
    p
}

/// Broadcast a gratuitous ARP on `ifindex`. Runs in the pod network
/// namespace.
#[allow(unsafe_code)]
fn announce(ifindex: u32, mac: MacAddr, ip: Ipv4Addr) -> io::Result<()> {
    let fd = bound_packet_socket(ifindex, ETH_P_ARP)?;
    let pkt = gratuitous_arp(mac, ip);
    // SAFETY: all-zero bytes are a valid sockaddr_ll.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as libc::c_ushort;
    sll.sll_protocol = ETH_P_ARP.to_be();
    sll.sll_ifindex = i32::try_from(ifindex).map_err(io::Error::other)?;
    sll.sll_halen = 6;
    sll.sll_addr[..6].copy_from_slice(&[0xff; 6]);
    // SAFETY: `pkt` is a valid buffer of the length passed, `sll` is a valid
    // initialized sockaddr_ll and its size is passed, `fd` is an open packet
    // socket.
    let n = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            pkt.as_ptr().cast(),
            pkt.len(),
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

#[allow(unsafe_code)]
fn bound_packet_socket(ifindex: u32, protocol: u16) -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let proto = protocol.to_be();
    // SAFETY: plain socket(2) call with constant arguments.
    let raw = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::c_int::from(proto),
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a new descriptor owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: all-zero bytes are a valid sockaddr_ll.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as libc::c_ushort;
    sll.sll_protocol = proto;
    sll.sll_ifindex = i32::try_from(ifindex).map_err(io::Error::other)?;
    // SAFETY: `sll` is a valid, initialized sockaddr_ll and the length passed
    // is its size. `fd` is an open packet socket.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const sll).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gratuitous_arp_layout() {
        let mac = MacAddr([0x02, 0x1f, 0x0c, 0x6c, 0x4a, 0x11]);
        let p = gratuitous_arp(mac, Ipv4Addr::new(10, 244, 1, 47));
        assert_eq!(&p[..8], &[0, 1, 8, 0, 6, 4, 0, 1]);
        assert_eq!(&p[8..14], &mac.0);
        assert_eq!(&p[14..18], &[10, 244, 1, 47]);
        assert_eq!(&p[18..24], &[0; 6]);
        assert_eq!(&p[24..28], &[10, 244, 1, 47]);
    }

    #[test]
    fn gact_parameters_are_twenty_bytes_with_ok_verdict() {
        let a = gact_ok();
        let TcActionAttribute::Options(opts) = &a.attributes[1] else {
            panic!("missing options");
        };
        let TcActionOption::Other(nla) = &opts[0] else {
            panic!("unexpected option");
        };
        use netlink_packet_core::Emitable;
        let mut buf = vec![0u8; nla.buffer_len()];
        nla.emit(&mut buf);
        // 4 byte NLA header, then 20 bytes of tc_gact.
        assert_eq!(buf.len(), 24);
        assert_eq!(i32::from_ne_bytes(buf[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn dhcp_filter_matches_udp_port_67() {
        let opts = dhcp_pass_filter();
        let TcFilterU32Option::Selector(sel) = &opts[0] else {
            panic!("missing selector");
        };
        assert_eq!(sel.nkeys, 2);
        assert_eq!(sel.keys[0].val & sel.keys[0].mask, 0x0011_0000);
        assert_eq!(sel.keys[1].val, 67);
    }
}
