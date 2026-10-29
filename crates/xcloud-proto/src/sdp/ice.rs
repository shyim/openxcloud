//! Port of qt/src/streaming/IceProcessor.{h,cpp}
//!
//! Teredo expansion for Xbox home streaming ICE candidates. Mirrors `ice.ts`:
//! Xbox home servers often send Teredo IPv6 candidates; the embedded IPv4 client
//! address and port are extracted to synthesise extra host candidates so NAT
//! traversal succeeds.

use std::net::Ipv6Addr;

use crate::models::IceCandidatePayload;

/// `IceProcessor`: stateless, kept as a type so call sites read like the C++.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IceProcessor;

impl IceProcessor {
    /// `expandCandidates`: expands remote candidates, adding synthetic host
    /// candidates for Teredo addresses. `a=end-of-candidates` entries are dropped.
    pub fn expand_candidates(
        &self,
        candidates: &[IceCandidatePayload],
    ) -> Vec<IceCandidatePayload> {
        expand_candidates(candidates)
    }

    /// See [`extract_teredo_endpoint`].
    pub fn extract_teredo_endpoint(ipv6: &str) -> Option<(String, u16)> {
        extract_teredo_endpoint(ipv6)
    }

    /// See [`expand_ipv6`].
    pub fn expand_ipv6(ipv6: &str) -> Option<[u8; 16]> {
        expand_ipv6(ipv6)
    }
}

/// `expandIpv6`: expands a (possibly compressed) IPv6 string into its 16 bytes.
/// `None` when the text is not an IPv6 address, including when it is an IPv4
/// address (`QHostAddress::protocol() != IPv6Protocol`).
///
/// Follows `QHostAddress::setAddress(const QString&)`: surrounding whitespace is
/// ignored (`QString::simplified`) and a trailing `%<scope id>` is stripped
/// before parsing; anything the parser rejects yields `None`.
pub fn expand_ipv6(ipv6: &str) -> Option<[u8; 16]> {
    let simplified = ipv6.trim();
    if simplified.is_empty() {
        return None;
    }
    // Qt only tries the IPv6 parser when a colon is present; IPv4 text falls
    // through to the IPv4 parser, whose result is rejected by the protocol check.
    if !simplified.contains(':') {
        return None;
    }
    let without_scope = simplified
        .rfind('%')
        .map_or(simplified, |scope| &simplified[..scope]);
    without_scope
        .parse::<Ipv6Addr>()
        .ok()
        .map(|address| address.octets())
}

/// `extractTeredoEndpoint`: the `(ipv4, port)` embedded in a Teredo address
/// (`2001::/32`): bytes 10-11 are the obfuscated port, bytes 12-15 the
/// obfuscated client IPv4, both XOR 0xFF. Like the C++, the prefix itself is
/// not checked here; the caller (`expand_candidates`) filters on `2001:`.
pub fn extract_teredo_endpoint(ipv6: &str) -> Option<(String, u16)> {
    let bytes = expand_ipv6(ipv6)?;

    let port_obfuscated = u16::from_be_bytes([bytes[10], bytes[11]]);
    let port = port_obfuscated ^ 0xFFFF;

    let a = bytes[12] ^ 0xFF;
    let b = bytes[13] ^ 0xFF;
    let c = bytes[14] ^ 0xFF;
    let d = bytes[15] ^ 0xFF;
    Some((format!("{a}.{b}.{c}.{d}"), port))
}

/// `IceProcessor::expandCandidates` as a free function.
pub fn expand_candidates(candidates: &[IceCandidatePayload]) -> Vec<IceCandidatePayload> {
    let mut expanded = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if candidate.candidate == "a=end-of-candidates" {
            continue;
        }

        let parts: Vec<&str> = candidate.candidate.split(' ').collect();
        let Some(&address) = parts.get(4) else {
            expanded.push(candidate.clone());
            continue;
        };

        if address.starts_with("2001:")
            && let Some((ipv4, port)) = extract_teredo_endpoint(address)
        {
            // Trailing spaces are part of the C++ format strings.
            expanded.push(IceCandidatePayload {
                candidate: format!("a=candidate:10 1 UDP 1 {ipv4} 9002 typ host "),
                sdp_mline_index: candidate.sdp_mline_index,
                sdp_mid: candidate.sdp_mid.clone(),
                ..IceCandidatePayload::default()
            });
            expanded.push(IceCandidatePayload {
                candidate: format!("a=candidate:11 1 UDP 1 {ipv4} {port} typ host "),
                sdp_mline_index: candidate.sdp_mline_index,
                sdp_mid: candidate.sdp_mid.clone(),
                ..IceCandidatePayload::default()
            });
        }

        expanded.push(candidate.clone());
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_teredo_endpoint() {
        // 63bf -> 0x63bf ^ 0xffff = 40000; 3fff:fdd2 -> 192.0.2.45
        let endpoint =
            IceProcessor::extract_teredo_endpoint("2001:0000:4136:e378:8000:63bf:3fff:fdd2");
        let (ipv4, port) = endpoint.expect("teredo endpoint");
        assert_eq!(ipv4, "192.0.2.45");
        assert_eq!(port, 40000);
    }

    #[test]
    fn rejects_non_teredo() {
        assert!(IceProcessor::extract_teredo_endpoint("not-an-address").is_none());
        assert!(IceProcessor::extract_teredo_endpoint("192.168.1.1").is_none());
    }

    #[test]
    fn expands_teredo_candidate() {
        let processor = IceProcessor;
        let candidate = IceCandidatePayload {
            candidate: "a=candidate:1 1 UDP 2130706431 2001:0000:4136:e378:8000:63bf:3fff:fdd2 9000 typ host"
                .to_owned(),
            sdp_mid: "0".to_owned(),
            sdp_mline_index: 0,
            ..IceCandidatePayload::default()
        };

        let expanded = processor.expand_candidates(std::slice::from_ref(&candidate));
        assert_eq!(expanded.len(), 3);
        assert!(expanded[0].candidate.contains("192.0.2.45 9002 typ host"));
        assert!(expanded[1].candidate.contains("192.0.2.45 40000 typ host"));
        assert_eq!(expanded[2].candidate, candidate.candidate);
    }

    #[test]
    fn passes_through_non_teredo() {
        let processor = IceProcessor;
        let candidate = IceCandidatePayload {
            candidate: "a=candidate:1 1 UDP 2130706431 192.168.1.5 54321 typ host".to_owned(),
            ..IceCandidatePayload::default()
        };
        let expanded = processor.expand_candidates(std::slice::from_ref(&candidate));
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].candidate, candidate.candidate);
    }

    // Additional checks for the QHostAddress parsing contract and the expansion details.

    #[test]
    fn expand_ipv6_matches_qhostaddress() {
        assert_eq!(
            expand_ipv6("2001:0000:4136:e378:8000:63bf:3fff:fdd2"),
            Some([
                0x20, 0x01, 0x00, 0x00, 0x41, 0x36, 0xe3, 0x78, 0x80, 0x00, 0x63, 0xbf, 0x3f, 0xff,
                0xfd, 0xd2
            ])
        );
        // Compressed form, surrounding whitespace, scope id and embedded IPv4 all parse.
        assert_eq!(
            expand_ipv6("::1"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
        assert_eq!(expand_ipv6("  2001::1  "), expand_ipv6("2001::1"));
        assert_eq!(expand_ipv6("fe80::1%en0"), expand_ipv6("fe80::1"));
        assert_eq!(
            expand_ipv6("::ffff:192.0.2.1"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 0, 2, 1])
        );
        // Not IPv6: IPv4, garbage, empty, too many groups, internal whitespace.
        assert!(expand_ipv6("192.168.1.1").is_none());
        assert!(expand_ipv6("not-an-address").is_none());
        assert!(expand_ipv6("").is_none());
        assert!(expand_ipv6("%en0").is_none());
        assert!(expand_ipv6("1:2:3:4:5:6:7:8:9").is_none());
        assert!(expand_ipv6("2001: :1").is_none());
    }

    #[test]
    fn synthetic_candidates_copy_mid_and_mline_index() {
        let candidate = IceCandidatePayload {
            candidate:
                "a=candidate:7 1 UDP 1 2001:0000:4136:e378:8000:63bf:3fff:fdd2 9000 typ host"
                    .to_owned(),
            sdp_mid: "video".to_owned(),
            sdp_mline_index: 1,
            username_fragment: "ufrag".to_owned(),
        };
        let expanded = expand_candidates(std::slice::from_ref(&candidate));
        assert_eq!(
            expanded[0].candidate,
            "a=candidate:10 1 UDP 1 192.0.2.45 9002 typ host "
        );
        assert_eq!(
            expanded[1].candidate,
            "a=candidate:11 1 UDP 1 192.0.2.45 40000 typ host "
        );
        for synthetic in &expanded[..2] {
            assert_eq!(synthetic.sdp_mid, "video");
            assert_eq!(synthetic.sdp_mline_index, 1);
            assert_eq!(synthetic.username_fragment, "");
        }
        assert_eq!(expanded[2], candidate);
    }

    #[test]
    fn drops_end_of_candidates_and_keeps_short_or_non_teredo_ipv6() {
        let end = IceCandidatePayload::new("a=end-of-candidates");
        let short = IceCandidatePayload::new("a=candidate:1 1 UDP 1");
        let other_ipv6 = IceCandidatePayload::new("a=candidate:1 1 UDP 1 2a02::1 9000 typ host");
        let bad_teredo = IceCandidatePayload::new("a=candidate:1 1 UDP 1 2001:zz 9000 typ host");
        let expanded =
            expand_candidates(&[end, short.clone(), other_ipv6.clone(), bad_teredo.clone()]);
        assert_eq!(expanded, vec![short, other_ipv6, bad_teredo]);
    }
}
