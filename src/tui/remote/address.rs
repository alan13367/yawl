//! Tailscale address discovery and pairing secrets.

use std::ffi::CStr;
use std::io::{self, Read};
use std::net::Ipv4Addr;

/// One IPv4 address assigned to a network interface.
struct InterfaceAddress {
    name: String,
    address: Ipv4Addr,
    netmask: Option<Ipv4Addr>,
}

/// Finds this machine's Tailscale IPv4 address.
pub(super) fn tailscale_ipv4() -> io::Result<Option<Ipv4Addr>> {
    Ok(interface_ipv4s()?
        .into_iter()
        .find(is_tailscale_interface)
        .map(|interface| interface.address))
}

/// 100.64.0.0/10 is the shared carrier-grade NAT range, also used by mobile
/// carriers and container networks, so the address alone is not enough.
/// Tailscale puts a single /32 address on its tunnel: `utun*` on macOS and
/// `tailscale*` on Linux.
fn is_tailscale_interface(interface: &InterfaceAddress) -> bool {
    let tunnel = interface.name.starts_with("utun") || interface.name.starts_with("tailscale");
    tunnel
        && interface.netmask == Some(Ipv4Addr::BROADCAST)
        && is_tailscale_range(interface.address)
}

fn is_tailscale_range(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 100 && (64..128).contains(&second)
}

fn interface_ipv4s() -> io::Result<Vec<InterfaceAddress>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `list` is valid writable storage for the list head, which
    // getifaddrs initializes on success.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut addresses = Vec::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: `entry` is a non-null node of the list returned by
        // getifaddrs, which stays alive until freeifaddrs below.
        let interface = unsafe { &*entry };
        let address = interface.ifa_addr;
        // SAFETY: A non-null `ifa_addr` points to a sockaddr whose family
        // field is always readable.
        if !address.is_null() && i32::from(unsafe { (*address).sa_family }) == libc::AF_INET {
            // SAFETY: AF_INET addresses are stored as sockaddr_in.
            let raw = unsafe { (*address.cast::<libc::sockaddr_in>()).sin_addr.s_addr };
            let netmask = interface.ifa_netmask;
            // SAFETY: A non-null netmask of an AF_INET entry is a
            // sockaddr_in that lives as long as the list.
            let netmask = (!netmask.is_null()).then(|| unsafe {
                Ipv4Addr::from(u32::from_be(
                    (*netmask.cast::<libc::sockaddr_in>()).sin_addr.s_addr,
                ))
            });
            // SAFETY: `ifa_name` is a NUL-terminated string owned by the list.
            let name = unsafe { CStr::from_ptr(interface.ifa_name) }
                .to_string_lossy()
                .into_owned();
            addresses.push(InterfaceAddress {
                name,
                address: Ipv4Addr::from(u32::from_be(raw)),
                netmask,
            });
        }
        entry = interface.ifa_next;
    }
    // SAFETY: `list` came from a successful getifaddrs call and no reference
    // into it outlives this point.
    unsafe { libc::freeifaddrs(list) };
    Ok(addresses)
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// A six-digit code short enough to type on a phone.
pub(super) fn pairing_code() -> io::Result<String> {
    let value = u32::from_le_bytes(random_bytes::<4>()?) % 1_000_000;
    Ok(format!("{value:06}"))
}

/// A 128-bit session token for the pairing cookie.
pub(super) fn session_token() -> io::Result<String> {
    Ok(random_bytes::<16>()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_tailscale_range() {
        assert!(is_tailscale_range(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailscale_range(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailscale_range(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_tailscale_range(Ipv4Addr::new(100, 63, 0, 1)));
        assert!(!is_tailscale_range(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn requires_a_tailscale_tunnel_not_just_a_cgnat_address() {
        let interface = |name: &str, netmask: [u8; 4]| InterfaceAddress {
            name: name.into(),
            address: Ipv4Addr::new(100, 101, 102, 103),
            netmask: Some(Ipv4Addr::from(netmask)),
        };
        assert!(is_tailscale_interface(&interface("utun6", [255; 4])));
        assert!(is_tailscale_interface(&interface("tailscale0", [255; 4])));
        // A carrier or container network sharing the range.
        assert!(!is_tailscale_interface(&interface("pdp_ip0", [255; 4])));
        assert!(!is_tailscale_interface(&interface(
            "cni0",
            [255, 192, 0, 0]
        )));
        assert!(!is_tailscale_interface(&interface(
            "utun6",
            [255, 192, 0, 0]
        )));
    }

    #[test]
    fn lists_interface_addresses() -> io::Result<()> {
        let loopback = interface_ipv4s()?
            .into_iter()
            .find(|interface| interface.address == Ipv4Addr::LOCALHOST)
            .expect("loopback");
        assert!(loopback.name.starts_with("lo"));
        assert_eq!(loopback.netmask, Some(Ipv4Addr::new(255, 0, 0, 0)));
        Ok(())
    }

    #[test]
    fn secrets_have_expected_shapes() -> io::Result<()> {
        let code = pairing_code()?;
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        let token = session_token()?;
        assert_eq!(token.len(), 32);
        assert_ne!(token, session_token()?);
        Ok(())
    }
}
