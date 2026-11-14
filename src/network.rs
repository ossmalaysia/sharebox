//! Discover candidates from the master's own interfaces, never an external service.
//! A private address is not evidence of peer reachability or a private firewall profile.
use serde::Serialize;
use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    bind_address: String,
    local_only: bool,
    urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
        || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()))
}

pub fn snapshot(bound: SocketAddr) -> NetworkInfo {
    // Local-only and unsupported listeners do not need interface enumeration.
    if loopback(bound.ip()) || bound.is_ipv6() {
        return describe(bound, Ok(Vec::new()));
    }
    let interfaces = if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .map(|interface| (interface.ip(), interface.is_oper_up()))
                .collect()
        })
        .map_err(|error| format!("Could not inspect local network interfaces: {error}"));
    describe(bound, interfaces)
}

fn describe(bound: SocketAddr, interfaces: Result<Vec<(IpAddr, bool)>, String>) -> NetworkInfo {
    let mut info = NetworkInfo {
        bind_address: bound.to_string(),
        local_only: loopback(bound.ip()),
        urls: Vec::new(),
        error: None,
    };
    if info.local_only {
        return info;
    }
    if bound.is_ipv6() {
        info.error = Some("Automatic LAN address discovery supports IPv4 listeners only. Use an IPv4 LAN bind address or 0.0.0.0 to share with another machine.".into());
        return info;
    }
    let interfaces = match interfaces {
        Ok(interfaces) => interfaces,
        Err(error) => {
            info.error = Some(error.chars().take(512).collect());
            return info;
        }
    };
    let addresses: BTreeSet<_> = interfaces
        .into_iter()
        .filter_map(|(ip, up)| match ip {
            IpAddr::V4(v4)
                if up && v4.is_private() && (bound.ip().is_unspecified() || ip == bound.ip()) =>
            {
                Some(v4)
            }
            _ => None,
        })
        .collect();
    info.urls = addresses
        .into_iter()
        .take(64)
        .map(|ip| format!("http://{ip}:{}", bound.port()))
        .collect();
    if info.urls.is_empty() {
        info.error = Some("No active private IPv4 address matches this listener. Check your LAN connection and master bind address.".into());
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interfaces() -> Vec<(IpAddr, bool)> {
        [
            ("127.0.0.1", true),
            ("0.0.0.0", true),
            ("169.254.10.2", true),
            ("100.64.0.1", true),
            ("8.8.8.8", true),
            ("224.0.0.1", true),
            ("192.168.1.4", true),
            ("192.168.1.4", true),
            ("192.168.1.5", false),
            ("10.2.3.4", true),
            ("172.16.1.2", true),
            ("172.32.1.2", true),
            ("fd00::1", true),
            ("fe80::1", true),
        ]
        .into_iter()
        .map(|(ip, up)| (ip.parse().unwrap(), up))
        .collect()
    }
    #[test]
    fn wildcard_lists_only_active_private_ipv4_and_uses_actual_port() {
        let info = describe("0.0.0.0:49152".parse().unwrap(), Ok(interfaces()));
        assert_eq!(info.bind_address, "0.0.0.0:49152");
        assert!(!info.local_only);
        assert!(info.error.is_none());
        assert_eq!(
            info.urls,
            [
                "http://10.2.3.4:49152",
                "http://172.16.1.2:49152",
                "http://192.168.1.4:49152"
            ]
        );
    }
    #[test]
    fn specific_bind_never_advertises_other_interfaces() {
        let info = describe("192.168.1.4:7331".parse().unwrap(), Ok(interfaces()));
        assert_eq!(info.urls, ["http://192.168.1.4:7331"]);
        for bind in ["192.168.1.5:7331", "192.168.1.6:7331", "8.8.8.8:7331"] {
            let info = describe(bind.parse().unwrap(), Ok(interfaces()));
            assert!(info.urls.is_empty());
            assert!(info.error.is_some());
            assert!(!info.local_only);
        }
    }
    #[test]
    fn loopback_is_local_even_when_discovery_fails() {
        for bind in [
            "127.0.0.1:7331",
            "127.2.3.4:7331",
            "[::1]:7331",
            "[::ffff:127.0.0.1]:7331",
        ] {
            let info = describe(bind.parse().unwrap(), Err("no adapters".into()));
            assert!(info.local_only);
            assert!(info.urls.is_empty());
            assert!(info.error.is_none());
        }
    }
    #[test]
    fn discovery_failure_and_ipv6_are_explicit_not_fallback_urls() {
        let failed = describe("0.0.0.0:7331".parse().unwrap(), Err("no adapters".into()));
        assert_eq!(failed.error.as_deref(), Some("no adapters"));
        assert!(failed.urls.is_empty());
        let ipv6 = describe("[::]:7331".parse().unwrap(), Ok(interfaces()));
        assert!(ipv6.urls.is_empty());
        assert!(!ipv6.local_only);
        assert!(ipv6.error.unwrap().contains("IPv4"));
    }
}
