//! Where `web_fetch` may connect: the public internet only. The model picks the URL, and a page it reads can carry
//! planted instructions; without this they could make it fetch something on the user's own machine or network: a
//! router's admin page, a debugging port (localhost:9222), a cloud metadata service (169.254.169.254) and the
//! credentials it hands out.
//!
//! Ports Marv's `src/tools/web/address.ts`; [`is_private_address`] fully, with its tests.
//!
//! - A host that is, or resolves to, any private address is refused (fail closed: no answer is no proof the name is
//!   public); `localhost` and `*.localhost` count as 127.0.0.1 without a lookup.
//! - Only "no such name" blames the address; a failing resolver is the network's fault, and the message says so.
//! - Lookups time out after [`DNS_TIMEOUT_MS`] = 10 s and stop with the run (Esc).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::types::ToolError;

pub const DNS_TIMEOUT_MS: u64 = 10_000;

fn private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0 // "this network"
        || a == 10
        || a == 127 // loopback
        || (a == 100 && (64..=127).contains(&b)) // carrier-grade NAT
        || (a == 169 && b == 254) // link-local, incl. cloud metadata
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || a >= 224 // multicast, reserved, broadcast
}

fn v4_of(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, (hi & 255) as u8, (lo >> 8) as u8, (lo & 255) as u8)
}

fn private_v6(ip: Ipv6Addr) -> bool {
    let g = ip.segments();
    let zero = |r: std::ops::Range<usize>| g[r].iter().all(|x| *x == 0);
    if zero(0..6) {
        return true; // ::, ::1, and the old IPv4-compatible form
    }
    if zero(0..5) && g[5] == 0xffff {
        return private_v4(v4_of(g[6], g[7])); // ::ffff:127.0.0.1
    }
    if zero(0..4) && g[4] == 0xffff && g[5] == 0 {
        return true; // ::ffff:0:0/96 (SIIT)
    }
    if g[0] == 0x64 && g[1] == 0xff9b && zero(2..6) {
        return private_v4(v4_of(g[6], g[7])); // NAT64
    }
    if g[0] == 0x64 && g[1] == 0xff9b && g[2] == 1 {
        return private_v4(v4_of(g[6], g[7])); // local-use NAT64 (64:ff9b:1::/48)
    }
    if g[0] == 0x2002 {
        return private_v4(v4_of(g[1], g[2])); // 6to4 embeds the v4
    }
    // unique local (fc00::/7), link-local (fe80::/10), old site-local (fec0::/10), multicast (ff00::/8)
    (g[0] & 0xfe00) == 0xfc00 || (g[0] & 0xffc0) == 0xfe80 || (g[0] & 0xffc0) == 0xfec0 || (g[0] & 0xff00) == 0xff00
}

/// Whether an address is on the user's own machine or network. Not an address at all: refused rather than guessed.
pub fn is_private_address(address: &str) -> bool {
    let ip = address.trim_start_matches('[').trim_end_matches(']');
    let ip = ip.split('%').next().unwrap_or(ip); // a zone ("fe80::1%eth0") isn't part of the address
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => private_v4(v4),
        Ok(IpAddr::V6(v6)) => private_v6(v6),
        Err(_) => true,
    }
}

/// Refuses a URL web_fetch mustn't fetch: anything but http(s), or a host that is, or resolves to, a private address.
pub async fn check_address(
    _url: &url::Url,
    _allow_private: bool,
    _cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), ToolError> {
    todo!(
        "port: checkAddress: scheme check, localhost shortcut, tokio::net::lookup_host with DNS_TIMEOUT_MS and cancel, fail closed, the 'Refused: … a private address' message"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "127.0.0.2",
            "10.0.0.1",
            "172.16.5.4",
            "172.31.255.255",
            "192.168.1.5",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "[::1]",
            "fe80::1",
            "fe80::1%eth0",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:7f00:1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
            "not an address",
            "192.0.0.1",
            "198.18.0.1",
            "198.19.255.255",
            "64:ff9b:1::a00:1",
            "::ffff:0:8.8.8.8",
            "2002:7f00:1::",
            "2002:c0a8:101::",
            "fec0::1",
            "feff::1",
        ] {
            assert!(is_private_address(ip), "{ip} is private");
        }
    }

    #[test]
    fn public_addresses() {
        for ip in [
            "93.184.215.14",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::6810:84e5",
            "::ffff:8.8.8.8",
            "172.15.255.255",
            "100.63.255.255",
            "169.253.0.1",
            "223.255.255.255",
            "198.17.255.255",
            "198.20.0.1",
            "64:ff9b::808:808",
            "2002:808:808::",
        ] {
            assert!(!is_private_address(ip), "{ip} is public");
        }
    }
}
