/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Relay hosts where `--insecure-skip-verify` is allowed (a dev stack with a self-signed cert).

use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

/// Link-local, carrier-grade NAT and any DNS name other than `localhost` are refused.
pub fn is_local_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => is_local_v4(ip),
        Some(Host::Ipv6(ip)) => is_local_v6(ip),
        None => false,
    }
}

fn is_local_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private()
}

fn is_local_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_local_v4(v4);
    }
    ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(s: &str) -> bool {
        is_local_host(&Url::parse(s).unwrap())
    }

    #[test]
    fn local_hosts() {
        for url in [
            "https://localhost:4433",
            "https://LOCALHOST",
            "https://127.0.0.1:4433",
            "https://127.8.9.10",
            "https://10.0.0.5",
            "https://172.16.0.1",
            "https://172.31.255.255",
            "https://192.168.1.20:4433",
            "https://[::1]:4433",
            "https://[fd12:3456::1]",
            "https://[::ffff:192.168.0.1]",
        ] {
            assert!(local(url), "{url} should be local");
        }
    }

    #[test]
    fn public_hosts() {
        for url in [
            "https://webtransport-us-east.webtransport.video",
            "https://localhost.evil.com",
            "https://8.8.8.8",
            "https://172.32.0.1",
            "https://192.169.0.1",
            "https://[2001:db8::1]",
            "https://[::ffff:8.8.8.8]",
            "https://raspberrypi.local",
            "https://169.254.1.1",
            "https://100.64.0.1",
            "https://[fe80::1]",
        ] {
            assert!(!local(url), "{url} should not be local");
        }
    }
}
