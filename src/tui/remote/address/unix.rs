//! Unix interface listing (`getifaddrs`) and random bytes (`/dev/urandom`).

use std::ffi::CStr;
use std::io::{self, Read};
use std::net::Ipv4Addr;

use super::InterfaceAddress;

pub(super) fn interface_ipv4s() -> io::Result<Vec<InterfaceAddress>> {
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
                // Tailscale's tunnel is not listed while it is down.
                up: true,
            });
        }
        entry = interface.ifa_next;
    }
    // SAFETY: `list` came from a successful getifaddrs call and no reference
    // into it outlives this point.
    unsafe { libc::freeifaddrs(list) };
    Ok(addresses)
}

pub(super) fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}
