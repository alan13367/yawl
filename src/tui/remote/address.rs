//! Tailscale address discovery and pairing secrets.

use std::io;
use std::net::Ipv4Addr;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use self::unix::{fill_random, interface_ipv4s};
#[cfg(windows)]
use self::windows::{fill_random, interface_ipv4s};

/// One IPv4 address assigned to a network interface.
struct InterfaceAddress {
    /// The interface name. Windows has no short names; there it is the
    /// adapter's friendly name followed by its description.
    name: String,
    address: Ipv4Addr,
    netmask: Option<Ipv4Addr>,
    /// The interface is operationally up.
    up: bool,
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
/// Tailscale puts a single /32 address on its tunnel: `utun*` on macOS,
/// `tailscale*` on Linux, and an adapter named Tailscale on Windows.
fn is_tailscale_interface(interface: &InterfaceAddress) -> bool {
    interface.up
        && is_tunnel_name(&interface.name)
        && interface.netmask == Some(Ipv4Addr::BROADCAST)
        && is_tailscale_range(interface.address)
}

#[cfg(unix)]
fn is_tunnel_name(name: &str) -> bool {
    is_unix_tunnel_name(name)
}

#[cfg(windows)]
fn is_tunnel_name(name: &str) -> bool {
    is_windows_tunnel_name(name)
}

#[cfg(any(unix, test))]
fn is_unix_tunnel_name(name: &str) -> bool {
    name.starts_with("utun") || name.starts_with("tailscale")
}

#[cfg(any(windows, test))]
fn is_windows_tunnel_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("tailscale")
}

fn is_tailscale_range(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 100 && (64..128).contains(&second)
}

/// The netmask for a prefix length, as an adapter reports it. Lengths above
/// 32 count as 32.
#[cfg(any(windows, test))]
fn prefix_netmask(length: u8) -> Ipv4Addr {
    let length = u32::from(length.min(32));
    Ipv4Addr::from(u32::MAX.checked_shl(32 - length).unwrap_or(0))
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    fill_random(&mut bytes)?;
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
            up: true,
        };
        // The platform's own rule decides the name; the rest is shared.
        let (tunnel, other) = if cfg!(windows) {
            ("Tailscale Tailscale Tunnel", "Ethernet Intel(R) Ethernet")
        } else {
            ("utun6", "pdp_ip0")
        };
        assert!(is_tailscale_interface(&interface(tunnel, [255; 4])));
        // A carrier or container network sharing the range.
        assert!(!is_tailscale_interface(&interface(other, [255; 4])));
        assert!(!is_tailscale_interface(&interface(
            "cni0",
            [255, 192, 0, 0]
        )));
        assert!(!is_tailscale_interface(&interface(
            tunnel,
            [255, 192, 0, 0]
        )));
    }

    #[test]
    fn tunnel_names_follow_each_platforms_convention() {
        assert!(is_unix_tunnel_name("utun6"));
        assert!(is_unix_tunnel_name("tailscale0"));
        assert!(!is_unix_tunnel_name("pdp_ip0"));
        assert!(!is_unix_tunnel_name("Tailscale"));

        assert!(is_windows_tunnel_name("Tailscale Tailscale Tunnel"));
        assert!(is_windows_tunnel_name("tailscale  "));
        assert!(!is_windows_tunnel_name(
            "Ethernet Intel(R) Ethernet Connection"
        ));
        assert!(!is_windows_tunnel_name("utun6"));
    }

    #[test]
    fn an_interface_that_is_down_is_not_chosen() {
        let mut interface = InterfaceAddress {
            name: if cfg!(windows) {
                "Tailscale Tailscale Tunnel".into()
            } else {
                "utun6".into()
            },
            address: Ipv4Addr::new(100, 101, 102, 103),
            netmask: Some(Ipv4Addr::BROADCAST),
            up: true,
        };
        assert!(is_tailscale_interface(&interface));
        interface.up = false;
        assert!(!is_tailscale_interface(&interface));
    }

    #[test]
    fn prefix_lengths_become_netmasks() {
        assert_eq!(prefix_netmask(32), Ipv4Addr::BROADCAST);
        assert_eq!(prefix_netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(prefix_netmask(10), Ipv4Addr::new(255, 192, 0, 0));
        assert_eq!(prefix_netmask(0), Ipv4Addr::UNSPECIFIED);
        assert_eq!(prefix_netmask(200), Ipv4Addr::BROADCAST);
    }

    #[cfg(unix)]
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

    #[cfg(windows)]
    #[test]
    fn lists_interface_addresses() -> io::Result<()> {
        // Every machine has a loopback adapter with 127.0.0.1/8.
        let loopback = interface_ipv4s()?
            .into_iter()
            .find(|interface| interface.address == Ipv4Addr::LOCALHOST)
            .expect("loopback");
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
