//! CIDR parsing and normalization for Nova LAN discovery.
//!
//! Discovery never scans the open Internet. Ranges come from the user, and a bare
//! `10.0.0.0` is ambiguous (the whole /8? one address?), so we refuse it instead of
//! guessing. Accepted shapes:
//!   - `10.0.0.0/8`, `100.64.0.0/10`            — explicit prefix
//!   - `10.0.0.0/24` … `/32`                     — any explicit prefix
//!   - `10.0.0.5`                                — a single host (treated as /32)
//!
//! A few small private ranges (`/24` of `192.168.0.0`, link-local `169.254.0.0/16`,
//! loopback) are recognized as private; anything outside RFC1918 + CGNAT + link-local
//! is refused unless the user explicitly allowlists it, so a typo like `8.8.8.0/24`
//! can't become a stray Internet scan.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Maximum prefix a user may ask to scan, per family. A /16 IPv4 is 65k addresses —
/// already more than a LAN scan should touch in one pass — and we cap total candidate
/// counts in the discovery layer on top of this.
pub const MAX_V4_PREFIX: u8 = 8;
pub const MAX_V6_PREFIX: u8 = 64;

/// A validated, normalized scan range.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CidrRange {
    /// Display form, always normalized to `addr/prefix`.
    pub cidr: String,
    pub family: AddrFamily,
    pub prefix: u8,
    /// First host in the range (network address).
    pub first: String,
    /// Last host in the range (broadcast).
    pub last: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AddrFamily {
    V4,
    V6,
}

/// Why a range was rejected — surfaced to the UI verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CidrError {
    #[error("empty input")]
    Empty,
    #[error("invalid address or CIDR: {0}")]
    Parse(String),
    #[error(
        "bare network address '{addr}' is ambiguous — add an explicit prefix, e.g. {addr}/{hint}"
    )]
    BareNetwork { addr: String, hint: u8 },
    #[error("prefix /{prefix} is too broad for scanning (max /{max}) — split into smaller ranges")]
    PrefixTooBroad { prefix: u8, max: u8 },
    #[error("{0} is not a private/LAN range; scanning it would touch the public Internet")]
    NotPrivate(String),
}

impl CidrRange {
    /// Parse and normalize a user-entered range string.
    ///
    /// `allow_public` should be `false` for LAN discovery (the default path) and `true`
    /// only when the user has explicitly opted into scanning a non-private target.
    pub fn parse(input: &str, allow_public: bool) -> Result<Self, CidrError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(CidrError::Empty);
        }

        let (addr_str, prefix_opt) = split_prefix(trimmed)?;
        // A bare host with no '/' may be a single IP (fine) or a bare network base like
        // "10.0.0.0" (ambiguous). Distinguish: if it parses as a host AND equals the
        // network base of some common prefix, treat the bare form as ambiguous.
        if prefix_opt.is_none() {
            if let Ok(v4) = addr_str.parse::<Ipv4Addr>() {
                if is_bare_v4_network(v4) {
                    return Err(CidrError::BareNetwork {
                        addr: v4.to_string(),
                        hint: suggested_v4_prefix(v4),
                    });
                }
                return single_host_v4(v4, allow_public);
            }
            if let Ok(v6) = addr_str.parse::<Ipv6Addr>() {
                return single_host_v6(v6, allow_public);
            }
            return Err(CidrError::Parse(trimmed.to_string()));
        }

        let prefix = prefix_opt.unwrap();
        if let Ok(v4) = addr_str.parse::<Ipv4Addr>() {
            return parse_v4_cidr(v4, prefix, allow_public);
        }
        if let Ok(v6) = addr_str.parse::<Ipv6Addr>() {
            return parse_v6_cidr(v6, prefix, allow_public);
        }
        Err(CidrError::Parse(trimmed.to_string()))
    }

    /// Iterate every host address in the range. Yields nothing for absurdly large
    /// ranges — the constructor caps prefixes, so this is bounded by `MAX_*_PREFIX`.
    pub fn hosts(&self) -> Vec<String> {
        match self.family {
            AddrFamily::V4 => {
                let lo = parse_u32(&self.first);
                let hi = parse_u32(&self.last);
                // Cap defensively at 65k hosts (a /16). Discovery caps the candidate
                // set earlier too, so this is belt-and-braces.
                (lo..=hi).take(65_536).map(u32_to_v4_string).collect()
            }
            AddrFamily::V6 => {
                // IPv6 ranges are enumerated as the /64 subnet-router anycast + ::1 in
                // practice; we do not fan out 18 quintillion addresses. Discovery uses
                // link-local / DNS-SD for v6, so return only the first and last host.
                vec![self.first.clone(), self.last.clone()]
            }
        }
    }
}

fn split_prefix(s: &str) -> Result<(&str, Option<u8>), CidrError> {
    match s.rfind('/') {
        Some(i) => {
            let (a, p) = (&s[..i], &s[i + 1..]);
            let prefix = p
                .parse::<u8>()
                .map_err(|_| CidrError::Parse(s.to_string()))?;
            Ok((a, Some(prefix)))
        }
        None => Ok((s, None)),
    }
}

fn is_bare_v4_network(v4: Ipv4Addr) -> bool {
    // The common "network base" forms users paste: .0.0 or .0 at the last octet for
    // classful-ish /24, /16, /8. Treat those as ambiguous so we never silently scan
    // a /8 because someone typed "10.0.0.0".
    let oct = v4.octets();
    oct == [10, 0, 0, 0]
        || oct == [172, 16, 0, 0]
        || oct == [192, 168, 0, 0]
        || oct == [100, 64, 0, 0]
        || (oct[3] == 0 && (oct[2] == 0 || oct[1] == 0))
}

fn suggested_v4_prefix(v4: Ipv4Addr) -> u8 {
    let oct = v4.octets();
    if oct[0] == 10 && oct[1] == 0 && oct[2] == 0 {
        8
    } else if oct[0] == 100 && oct[1] == 64 && oct[2] == 0 {
        10
    } else if oct[0] == 172 && oct[2] == 0 {
        12
    } else if oct[0] == 192 && oct[1] == 168 && oct[2] == 0 {
        16
    } else {
        24
    }
}

fn single_host_v4(v4: Ipv4Addr, allow_public: bool) -> Result<CidrRange, CidrError> {
    parse_v4_cidr(v4, 32, allow_public)
}

fn single_host_v6(v6: Ipv6Addr, allow_public: bool) -> Result<CidrRange, CidrError> {
    parse_v6_cidr(v6, 128, allow_public)
}

fn parse_v4_cidr(v4: Ipv4Addr, prefix: u8, allow_public: bool) -> Result<CidrRange, CidrError> {
    if prefix > 32 {
        return Err(CidrError::Parse(format!("{v4}/{prefix}")));
    }
    if prefix < MAX_V4_PREFIX {
        return Err(CidrError::PrefixTooBroad {
            prefix,
            max: MAX_V4_PREFIX,
        });
    }
    let bits = u32::from(v4);
    let mask = if prefix == 0 {
        0
    } else {
        (!0u32) << (32 - prefix)
    };
    let net = bits & mask;
    let broadcast = net | !mask;
    let base = Ipv4Addr::from(net);
    let bcast = Ipv4Addr::from(broadcast);
    let is_priv = is_private_v4(base) && is_private_v4(bcast);
    if !is_priv && !allow_public {
        return Err(CidrError::NotPrivate(format!("{base}/{prefix}")));
    }
    // Normalize the displayed address to the network base.
    Ok(CidrRange {
        cidr: format!("{base}/{prefix}"),
        family: AddrFamily::V4,
        prefix,
        first: base.to_string(),
        last: bcast.to_string(),
    })
}

fn parse_v6_cidr(v6: Ipv6Addr, prefix: u8, allow_public: bool) -> Result<CidrRange, CidrError> {
    if prefix > 128 {
        return Err(CidrError::Parse(format!("{v6}/{prefix}")));
    }
    if prefix < MAX_V6_PREFIX {
        return Err(CidrError::PrefixTooBroad {
            prefix,
            max: MAX_V6_PREFIX,
        });
    }
    let is_priv = is_private_v6(v6);
    if !is_priv && !allow_public {
        return Err(CidrError::NotPrivate(format!("{v6}/{prefix}")));
    }
    Ok(CidrRange {
        cidr: format!("{v6}/{prefix}"),
        family: AddrFamily::V6,
        prefix,
        first: v6.to_string(),
        last: v6.to_string(),
    })
}

fn is_private_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    o[0] == 10 // RFC1918
        || (o[0] == 172 && (16..=31).contains(&o[1])) // RFC1918
        || (o[0] == 192 && o[1] == 168) // RFC1918
        || (o[0] == 100 && (64..=127).contains(&o[1])) // RFC6598 CGNAT (Tailscale-ish)
        || (o[0] == 169 && o[1] == 254) // link-local
        || o[0] == 127 // loopback
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // TEST-NET-1 (doc) — allow for tests
}

fn is_private_v6(a: Ipv6Addr) -> bool {
    let seg = a.segments();
    (seg[0] & 0xFE00) == 0xFC00 // ULA fc00::/7
        || seg[0] == 0xfe80 // link-local
        || a.is_loopback()
}

fn parse_u32(s: &str) -> u32 {
    u32::from(s.parse::<Ipv4Addr>().unwrap_or(Ipv4Addr::UNSPECIFIED))
}

fn u32_to_v4_string(n: u32) -> String {
    Ipv4Addr::from(n).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_rfc1918_and_cgnat() {
        let r = CidrRange::parse("10.0.0.0/8", false).unwrap();
        assert_eq!(r.prefix, 8);
        assert_eq!(r.family, AddrFamily::V4);
        // /8 is allowed (== MAX_V4_PREFIX) — the cap is "no broader than".
        assert_eq!(r.cidr, "10.0.0.0/8");

        let r = CidrRange::parse("100.64.0.0/10", false).unwrap();
        assert_eq!(r.prefix, 10);
        assert_eq!(r.first, "100.64.0.0");
        assert_eq!(r.last, "100.127.255.255");
    }

    #[test]
    fn rejects_bare_network_as_ambiguous() {
        match CidrRange::parse("10.0.0.0", false) {
            Err(CidrError::BareNetwork { addr, hint }) => {
                assert_eq!(addr, "10.0.0.0");
                assert_eq!(hint, 8);
            }
            other => panic!("expected BareNetwork, got {other:?}"),
        }
        match CidrRange::parse("100.64.0.0", false) {
            Err(CidrError::BareNetwork { hint, .. }) => assert_eq!(hint, 10),
            other => panic!("expected BareNetwork, got {other:?}"),
        }
    }

    #[test]
    fn single_host_is_accepted() {
        let r = CidrRange::parse("10.0.0.5", false).unwrap();
        assert_eq!(r.prefix, 32);
        assert_eq!(r.first, "10.0.0.5");
    }

    #[test]
    fn rejects_too_broad_prefix() {
        assert!(matches!(
            CidrRange::parse("10.0.0.0/7", false),
            Err(CidrError::PrefixTooBroad { prefix: 7, max: 8 })
        ));
    }

    #[test]
    fn rejects_public_without_opt_in() {
        // 8.8.8.0/24 is public.
        assert!(matches!(
            CidrRange::parse("8.8.8.0/24", false),
            Err(CidrError::NotPrivate(_))
        ));
        // With explicit opt-in it's allowed (the user knows what they're doing).
        assert!(CidrRange::parse("8.8.8.0/24", true).is_ok());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(CidrRange::parse("", false), Err(CidrError::Empty)));
        assert!(CidrRange::parse("not an ip", false).is_err());
        assert!(CidrRange::parse("10.0.0.0/99", false).is_err());
    }

    #[test]
    fn hosts_enumerates_a_small_range() {
        let r = CidrRange::parse("192.168.1.0/30", false).unwrap();
        let hosts = r.hosts();
        // /30 = 4 addresses (.0 network .. .3 broadcast) — discovery will filter these.
        assert_eq!(
            hosts,
            vec!["192.168.1.0", "192.168.1.1", "192.168.1.2", "192.168.1.3"]
        );
    }

    #[test]
    fn normalizes_non_canonical_base() {
        // 10.1.2.3/8 normalizes the display to the network base but keeps /8.
        let r = CidrRange::parse("10.1.2.3/8", false).unwrap();
        assert_eq!(r.cidr, "10.0.0.0/8");
        assert_eq!(r.first, "10.0.0.0");
        assert_eq!(r.last, "10.255.255.255");
    }

    #[test]
    fn link_local_and_ula_accepted() {
        assert!(CidrRange::parse("169.254.0.0/16", false).is_ok());
        assert!(CidrRange::parse("fd00::/64", false).is_ok());
    }
}
