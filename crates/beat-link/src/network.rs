//! Network plumbing: port configuration, socket creation, and interface discovery.

use crate::util;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// The UDP ports used for the protocol. Real devices always use the defaults; tests
/// use alternate ports so they can run without privileges or real hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub announcement: u16,
    pub beat: u16,
    pub update: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Ports { announcement: util::ANNOUNCEMENT_PORT, beat: util::BEAT_PORT, update: util::UPDATE_PORT }
    }
}

/// Configuration shared by the protocol components.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// Ports to listen on and send to.
    pub ports: Ports,
    /// Address to bind the listening sockets to (normally all interfaces).
    pub bind_address: Ipv4Addr,
    /// When set, skip interface matching and use this local address / broadcast
    /// address pair (used by tests on loopback, and by users with unusual setups).
    pub forced_interface: Option<InterfaceAddress>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig { ports: Ports::default(), bind_address: Ipv4Addr::UNSPECIFIED, forced_interface: None }
    }
}

/// An IPv4 address assigned to a local network interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub interface_name: String,
    pub address: Ipv4Addr,
    pub prefix_length: u8,
    pub broadcast: Ipv4Addr,
    pub hardware_address: Option<[u8; 6]>,
}

fn prefix_from_netmask(mask: Ipv4Addr) -> u8 {
    u32::from(mask).count_ones() as u8
}

/// List every IPv4 interface address that has a broadcast address.
pub fn interface_addresses() -> Vec<InterfaceAddress> {
    let mut result = Vec::new();
    match if_addrs::get_if_addrs() {
        Ok(addrs) => {
            for iface in addrs {
                if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                    let prefix = prefix_from_netmask(v4.netmask);
                    let broadcast = v4.broadcast.unwrap_or_else(|| {
                        let mask = u32::from(v4.netmask);
                        Ipv4Addr::from(u32::from(v4.ip) | !mask)
                    });
                    let hw = mac_address::mac_address_by_name(&iface.name).ok().flatten().map(|m| m.bytes());
                    result.push(InterfaceAddress {
                        interface_name: iface.name.clone(),
                        address: v4.ip,
                        prefix_length: prefix,
                        broadcast,
                        hardware_address: hw,
                    });
                }
            }
        }
        Err(e) => log::error!("Unable to enumerate network interfaces: {e}"),
    }
    result
}

/// Find the local interface addresses that are on the same network as `device`.
pub fn matching_addresses(device: Ipv4Addr) -> Vec<InterfaceAddress> {
    interface_addresses()
        .into_iter()
        .filter(|a| !a.address.is_loopback() || device.is_loopback())
        .filter(|a| util::same_network(a.prefix_length, a.address, device))
        .collect()
}

/// Describe the interfaces, for troubleshooting log messages.
pub fn describe_interfaces() -> Vec<String> {
    let mut lines: Vec<String> = interface_addresses()
        .into_iter()
        .map(|a| format!("{}: {}/{}", a.interface_name, a.address, a.prefix_length))
        .collect();
    lines.sort();
    lines
}

/// Create a UDP socket bound to `addr` with address reuse and broadcast enabled, and
/// a short read timeout so receiver threads can notice shutdown promptly.
pub fn bind_udp(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    {
        let _ = socket.set_reuse_port(true);
    }
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddr::V4(addr).into())?;
    let udp: UdpSocket = socket.into();
    udp.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(udp)
}

/// Extract the IPv4 address from a socket address (mapping IPv6-wrapped IPv4).
pub fn ipv4_of(addr: &SocketAddr) -> Option<Ipv4Addr> {
    match addr {
        SocketAddr::V4(a) => Some(*a.ip()),
        SocketAddr::V6(a) => a.ip().to_ipv4_mapped().or_else(|| a.ip().to_ipv4()),
    }
}

/// Returns true for errors that just mean a read timed out.
pub fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netmask_prefix() {
        assert_eq!(prefix_from_netmask(Ipv4Addr::new(255, 255, 255, 0)), 24);
        assert_eq!(prefix_from_netmask(Ipv4Addr::new(255, 255, 0, 0)), 16);
    }

    #[test]
    fn can_bind_ephemeral() {
        let s = bind_udp(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(s.local_addr().unwrap().port() > 0);
    }
}
