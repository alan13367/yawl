//! Windows interface listing (`GetAdaptersAddresses`) and random bytes
//! (`BCryptGenRandom`).

use std::io;
use std::net::Ipv4Addr;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, ERROR_SUCCESS};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

use super::{InterfaceAddress, prefix_netmask};

/// The size Microsoft recommends for a first attempt.
const INITIAL_BYTES: u32 = 15_000;
/// Adapters can appear between the size query and the fill; a few retries
/// settle that.
const ATTEMPTS: usize = 4;
/// Adapter names are far shorter; the bound only guards a corrupt string.
const MAX_NAME_UNITS: usize = 512;

/// Lists IPv4 unicast addresses of all adapters. `name` joins the adapter's
/// friendly name and description, such as `Tailscale Tailscale Tunnel`.
pub(super) fn interface_ipv4s() -> io::Result<Vec<InterfaceAddress>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut wanted = INITIAL_BYTES;
    for _ in 0..ATTEMPTS {
        // u64 elements keep the buffer 8-byte aligned for the pointers inside.
        let mut buffer = vec![0u64; (wanted as usize).div_ceil(8)];
        let mut length = u32::try_from(buffer.len() * 8).map_err(io::Error::other)?;
        let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: `head` points to `length` writable bytes, suitably aligned
        // for the adapter structures, and `length` is writable storage for
        // the size the call needs when the buffer is too small. No reserved
        // pointer is passed.
        let status = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                flags,
                std::ptr::null(),
                head,
                &mut length,
            )
        };
        match status {
            // SAFETY: The call succeeded, so `head` is the first node of a
            // list inside `buffer`, which lives to the end of this iteration.
            ERROR_SUCCESS => return Ok(unsafe { collect(head) }),
            ERROR_NO_DATA => return Ok(Vec::new()),
            ERROR_BUFFER_OVERFLOW => wanted = length.max(INITIAL_BYTES),
            error => return Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
    Err(io::Error::other("the network adapter list kept changing"))
}

/// Walks the adapter list and its IPv4 unicast addresses.
///
/// # Safety
///
/// `head` must be the first node of a list written by a successful
/// `GetAdaptersAddresses` call, in a buffer that outlives this call.
unsafe fn collect(head: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<InterfaceAddress> {
    let mut addresses = Vec::new();
    let mut adapter = head;
    while !adapter.is_null() {
        // SAFETY: Non-null nodes of the list stay valid with the buffer.
        let current = unsafe { &*adapter };
        let up = current.OperStatus == IfOperStatusUp;
        // SAFETY: The name pointers are null or NUL-terminated UTF-16 inside
        // the buffer.
        let name = unsafe {
            format!(
                "{} {}",
                wide_string(current.FriendlyName),
                wide_string(current.Description)
            )
        };
        let mut unicast = current.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: Non-null nodes of the unicast list stay valid with the
            // buffer.
            let entry = unsafe { &*unicast };
            let socket = entry.Address.lpSockaddr;
            let fits = usize::try_from(entry.Address.iSockaddrLength)
                .is_ok_and(|length| length >= size_of::<SOCKADDR_IN>());
            // SAFETY: A non-null `lpSockaddr` points to a socket address
            // whose family field is readable.
            if !socket.is_null() && fits && unsafe { (*socket).sa_family } == AF_INET {
                // SAFETY: The family is AF_INET and the length covers a
                // SOCKADDR_IN, which the union read stays inside.
                let raw = unsafe { (*socket.cast::<SOCKADDR_IN>()).sin_addr.S_un.S_addr };
                addresses.push(InterfaceAddress {
                    name: name.clone(),
                    address: Ipv4Addr::from(u32::from_be(raw)),
                    netmask: Some(prefix_netmask(entry.OnLinkPrefixLength)),
                    up,
                });
            }
            unicast = entry.Next;
        }
        adapter = current.Next;
    }
    addresses
}

/// A NUL-terminated UTF-16 string, empty for a null pointer.
///
/// # Safety
///
/// `text` must be null or point to a NUL-terminated UTF-16 string.
unsafe fn wide_string(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut length = 0;
    // SAFETY: The string is NUL-terminated, so reads stop at the terminator.
    while length < MAX_NAME_UNITS && unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: The first `length` units were just read.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) })
}

pub(super) fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    // SAFETY: `bytes` is writable for `length` bytes. A null algorithm handle
    // with BCRYPT_USE_SYSTEM_PREFERRED_RNG selects the system generator.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    // NTSTATUS values below zero are errors.
    if status < 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed with status {status:#010x}"
        )));
    }
    Ok(())
}
