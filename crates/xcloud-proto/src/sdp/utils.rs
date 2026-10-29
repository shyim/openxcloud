//! Port of qt/src/streaming/SdpUtils.{h,cpp}
//!
//! SDP helpers ported from green-nx (`src/core/session.cpp` and
//! `src/switch/stream/engine.cpp`). Every transform here edits the offer in
//! place and byte for byte: line endings (CRLF or LF) are left exactly as the
//! caller supplied them, preserving the original signaling contract.

/// Index of the first `\r` or `\n` at or after `from`, or `sdp.len()`.
fn line_end(sdp: &str, from: usize) -> usize {
    sdp[from..]
        .find(['\r', '\n'])
        .map_or(sdp.len(), |offset| from + offset)
}

/// `localCandidatesFromSdp`: extracts the `candidate:...` lines (without the
/// `a=` prefix) from a local offer for the `/ice` POST.
pub fn local_candidates_from_sdp(sdp: &str) -> Vec<String> {
    const NEEDLE: &str = "a=candidate:";
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(offset) = sdp[at..].find(NEEDLE) {
        let start = at + offset;
        let end = line_end(sdp, start);
        out.push(sdp[start + 2..end].to_owned());
        at = end;
    }
    out
}

/// `ufragFromSdp`: the first `a=ice-ufrag:` value, used to tag outbound ICE
/// candidates. Empty when the offer carries none (Qt returned a null `QString`).
pub fn ufrag_from_sdp(sdp: &str) -> String {
    const NEEDLE: &str = "a=ice-ufrag:";
    let Some(at) = sdp.find(NEEDLE) else {
        return String::new();
    };
    let start = at + NEEDLE.len();
    let end = line_end(sdp, start);
    sdp[start..end].to_owned()
}

/// `forceOpusStereo`: adds `stereo=1` after `useinbandfec=1` on the Opus fmtp
/// line when the offer does not already mention `stereo=1` anywhere.
pub fn force_opus_stereo(sdp: &str) -> String {
    const NEEDLE: &str = "useinbandfec=1";
    let mut result = sdp.to_owned();
    if let Some(at) = result.find(NEEDLE)
        && !result.contains("stereo=1")
    {
        result.insert_str(at + NEEDLE.len(), ";stereo=1");
    }
    result
}

/// `scaleVideoCaps1080`: scales the 720p decode capability (`max-fs=3600`,
/// `max-mbps=108000`) to 1920x1080@60 (`max-fs=8160`, `max-mbps=489600`).
/// Same-length in-place edits.
pub fn scale_video_caps_1080(sdp: &str) -> String {
    let mut out = sdp.to_owned();
    if let Some(at) = out.find("max-fs=3600") {
        // "max-fs=" is 7 bytes, the value 4.
        out.replace_range(at + 7..at + 11, "8160");
    }
    if let Some(at) = out.find("max-mbps=108000") {
        // "max-mbps=" is 9 bytes, the value 6.
        out.replace_range(at + 9..at + 15, "489600");
    }
    out
}

/// `setH264ProfileLevelId`: replaces the first `profile-level-id=[0-9a-fA-F]{6}`
/// with `profile_level_id`. The home (remote play) console agent expects level
/// 3.2 (`42e020`), unlike xCloud.
///
/// Equivalent to the C++ `QRegularExpression` match: the first occurrence of
/// `profile-level-id=` that is followed by six hex digits is rewritten; any
/// further hex digits after the sixth are kept, as the regex would leave them.
pub fn set_h264_profile_level_id(sdp: &str, profile_level_id: &str) -> String {
    const NEEDLE: &str = "profile-level-id=";
    let mut out = sdp.to_owned();
    let mut search = 0;
    while let Some(offset) = out[search..].find(NEEDLE) {
        let start = search + offset;
        let value = start + NEEDLE.len();
        let is_match = out
            .as_bytes()
            .get(value..value + 6)
            .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
        if is_match {
            out.replace_range(start..value + 6, &format!("{NEEDLE}{profile_level_id}"));
            break;
        }
        search = value;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
        a=ice-ufrag:AbCd\r\n\
        a=ice-pwd:secret\r\n\
        a=candidate:1 1 UDP 2122260223 192.168.1.5 54321 typ host\r\n\
        a=candidate:2 1 UDP 1686052607 203.0.113.7 9000 typ srflx\r\n\
        a=fmtp:111 minptime=10;useinbandfec=1\r\n\
        a=fmtp:102 level-asymmetry-allowed=0;packetization-mode=1;\
        profile-level-id=42e01f;max-fs=3600;max-mbps=108000\r\n";

    #[test]
    fn extracts_local_candidates() {
        let candidates = local_candidates_from_sdp(OFFER);
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].starts_with("candidate:1 1 UDP"));
        assert!(candidates[1].contains("203.0.113.7"));
    }

    #[test]
    fn extracts_ufrag() {
        assert_eq!(ufrag_from_sdp(OFFER), "AbCd");
        assert_eq!(ufrag_from_sdp("no ufrag here"), "");
    }

    #[test]
    fn forces_opus_stereo() {
        let result = force_opus_stereo(OFFER);
        assert!(result.contains("useinbandfec=1;stereo=1"));
        // Idempotent.
        let twice = force_opus_stereo(&result);
        assert_eq!(twice.matches("stereo=1").count(), 1);
    }

    #[test]
    fn scales_video_caps() {
        let result = scale_video_caps_1080(OFFER);
        assert!(result.contains("max-fs=8160"));
        assert!(result.contains("max-mbps=489600"));
        assert!(!result.contains("max-fs=3600"));
    }

    #[test]
    fn replaces_h264_level() {
        let result = set_h264_profile_level_id(OFFER, "42e020");
        assert!(result.contains("profile-level-id=42e020"));
        assert!(!result.contains("profile-level-id=42e01f"));
    }

    // Additional checks for the line-ending contract and the hand-rolled regex.

    #[test]
    fn transforms_keep_crlf_untouched() {
        let stereo = force_opus_stereo(OFFER);
        assert_eq!(
            stereo.matches("\r\n").count(),
            OFFER.matches("\r\n").count()
        );
        assert!(!stereo.contains("\n\n"));
        let scaled = scale_video_caps_1080(OFFER);
        assert_eq!(scaled.len(), OFFER.len());
        assert!(scaled.ends_with("max-mbps=489600\r\n"));
        let level = set_h264_profile_level_id(OFFER, "42e020");
        assert_eq!(level.len(), OFFER.len());
    }

    #[test]
    fn candidates_and_ufrag_handle_lf_and_unterminated_lines() {
        let sdp = "a=ice-ufrag:xyz\na=candidate:1 1 UDP 1 1.2.3.4 9 typ host";
        assert_eq!(ufrag_from_sdp(sdp), "xyz");
        assert_eq!(
            local_candidates_from_sdp(sdp),
            vec!["candidate:1 1 UDP 1 1.2.3.4 9 typ host".to_owned()]
        );
        assert_eq!(ufrag_from_sdp("a=ice-ufrag:"), "");
    }

    #[test]
    fn profile_level_id_requires_six_hex_digits() {
        // A non-hex value is skipped; the next valid occurrence is rewritten.
        let sdp = "a=fmtp:96 profile-level-id=zz001f\r\na=fmtp:97 profile-level-id=42E01F;x=1\r\n";
        let result = set_h264_profile_level_id(sdp, "640029");
        assert!(result.contains("profile-level-id=zz001f"));
        assert!(result.contains("a=fmtp:97 profile-level-id=640029;x=1\r\n"));
        // Only the first match is rewritten.
        let sdp = "profile-level-id=42e01f profile-level-id=42e01f";
        assert_eq!(
            set_h264_profile_level_id(sdp, "42e020"),
            "profile-level-id=42e020 profile-level-id=42e01f"
        );
        // No match leaves the input untouched.
        assert_eq!(set_h264_profile_level_id("v=0\r\n", "42e020"), "v=0\r\n");
        assert_eq!(
            set_h264_profile_level_id("profile-level-id=4", "42e020"),
            "profile-level-id=4"
        );
    }
}
