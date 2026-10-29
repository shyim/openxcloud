//! Port of qt/src/streaming/SdpProcessor.{h,cpp}
//!
//! Local SDP offer tuning required by the xCloud service. Mirrors `sdp.ts`:
//! codec reordering, bitrate caps, max frame rate, H.264 profile override and
//! Opus stereo.
//!
//! Line-ending contract (identical to the C++): every transform in this module
//! works on a line list built by `splitLines`, which normalises CRLF and lone CR
//! to LF, and re-joins with `\n`. So a transform that *changes* the offer returns
//! it LF-terminated, while a transform that finds nothing to do returns the
//! input untouched (CRLF preserved). The in-place edits that must keep CRLF live
//! in [`super::utils`].

use std::collections::{HashMap, HashSet};

use super::utils;

/// `SdpProcessor::ProcessOptions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOptions {
    pub max_video_bitrate_kbps: u32,
    pub max_audio_bitrate_kbps: u32,
    pub stereo_audio: bool,
    /// `"VP9"`, `"VP8"`, ...; empty keeps H264 first.
    pub preferred_video_codec: String,
    pub max_video_frame_rate: u32,
    pub h264_profile_level_id_override: String,
    /// When false, `b=AS` lines are omitted (the native client path).
    pub inject_bitrate_lines: bool,
    /// Scale the 720p decode capability in the offer to 1080p60.
    pub scale_video_caps_to_1080: bool,
    /// Replace the H.264 profile-level-id verbatim (home streams want 42e020).
    pub h264_profile_level_id: String,
}

impl Default for ProcessOptions {
    fn default() -> Self {
        Self {
            max_video_bitrate_kbps: 0,
            max_audio_bitrate_kbps: 0,
            stereo_audio: true,
            preferred_video_codec: String::new(),
            max_video_frame_rate: 0,
            h264_profile_level_id_override: String::new(),
            inject_bitrate_lines: true,
            scale_video_caps_to_1080: false,
            h264_profile_level_id: String::new(),
        }
    }
}

/// `splitLines`: CRLF and CR become LF, then split on LF (empty parts kept, like
/// `QString::split` with `Qt::KeepEmptyParts`).
fn split_lines(sdp: &str) -> Vec<String> {
    sdp.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_owned)
        .collect()
}

/// `QString::section(sep, 0, 0)`: the text before the first `sep`.
fn first_section(text: &str, sep: char) -> &str {
    text.split(sep).next().unwrap_or("")
}

/// Builds the payload-type to codec-name (upper-cased) map for the video section.
fn video_payload_types(lines: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut in_video_section = false;
    for raw in lines {
        let line = raw.trim();
        if let Some(media) = line.strip_prefix("m=") {
            in_video_section = media.starts_with("video");
            continue;
        }
        if !in_video_section {
            continue;
        }
        let Some(rest) = line.strip_prefix("a=rtpmap:") else {
            continue;
        };
        let mut parts = rest.split(' ');
        if let (Some(pt), Some(codec)) = (parts.next(), parts.next()) {
            map.insert(pt.to_owned(), first_section(codec, '/').to_uppercase());
        }
    }
    map
}

/// `QHash::value(pt)`: the mapped codec or an empty string.
fn codec_for<'a>(map: &'a HashMap<String, String>, pt: &str) -> &'a str {
    map.get(pt).map_or("", String::as_str)
}

/// `h264ProfilePriority`: 0 High, 1 constrained baseline, 2 baseline, 99 other.
pub fn h264_profile_priority(fmtp: &str) -> i32 {
    if fmtp.contains("profile-level-id=4d") {
        0
    } else if fmtp.contains("profile-level-id=42e") {
        1
    } else if fmtp.contains("profile-level-id=420") {
        2
    } else {
        99
    }
}

/// `reorderVideoCodecs`: moves the payload types of `preferred_codec_name` to
/// the front of the `m=video` line.
pub fn reorder_video_codecs(sdp: &str, preferred_codec_name: &str) -> String {
    let mut lines = split_lines(sdp);
    let Some(video_index) = lines.iter().position(|line| line.starts_with("m=video")) else {
        return sdp.to_owned();
    };

    let components: Vec<&str> = lines[video_index].trim().split(' ').collect();
    if components.len() <= 3 {
        return sdp.to_owned();
    }

    let pt_to_codec = video_payload_types(&lines);
    let preferred = preferred_codec_name.to_uppercase();

    let mut preferred_pts = Vec::new();
    let mut other_pts = Vec::new();
    for &pt in &components[3..] {
        if codec_for(&pt_to_codec, pt).to_uppercase() == preferred {
            preferred_pts.push(pt);
        } else {
            other_pts.push(pt);
        }
    }
    if preferred_pts.is_empty() {
        return sdp.to_owned();
    }

    let mut reordered: Vec<&str> = components[..3].to_vec();
    reordered.extend(preferred_pts);
    reordered.extend(other_pts);
    let video_line = reordered.join(" ");
    lines[video_index] = video_line;
    lines.join("\n")
}

/// `setBitrate`: inserts (or replaces) a `b=AS:<kbps>` line after the `i=`/`c=`
/// lines of the `m=<media_type>` section, matching `sdp.ts`.
pub fn set_bitrate(sdp: &str, media_type: &str, bitrate_kbps: u32) -> String {
    let mut lines = split_lines(sdp);
    let prefix = format!("m={media_type}");
    let Some(media_index) = lines.iter().position(|line| line.starts_with(&prefix)) else {
        return sdp.to_owned();
    };

    let mut insert_index = media_index + 1;
    while insert_index < lines.len()
        && (lines[insert_index].starts_with("i=") || lines[insert_index].starts_with("c="))
    {
        insert_index += 1;
    }

    let bitrate_line = format!("b=AS:{bitrate_kbps}");
    if insert_index < lines.len() && lines[insert_index].starts_with("b=") {
        lines[insert_index] = bitrate_line;
    } else {
        lines.insert(insert_index, bitrate_line);
    }
    lines.join("\n")
}

/// `setVideoMaxFrameRate`: sets `a=max-fr:<fps>` in the video section, replacing
/// an existing one or inserting it after the `c=`/`i=`/`b=` lines.
pub fn set_video_max_frame_rate(sdp: &str, max_frame_rate: u32) -> String {
    let mut lines = split_lines(sdp);
    let Some(media_start) = lines.iter().position(|line| line.starts_with("m=video")) else {
        return sdp.to_owned();
    };

    let media_end = lines
        .iter()
        .skip(media_start + 1)
        .position(|line| line.starts_with("m="))
        .map_or(lines.len(), |offset| media_start + 1 + offset);

    let frame_rate_line = format!("a=max-fr:{max_frame_rate}");
    if let Some(existing) = lines[media_start..media_end]
        .iter()
        .position(|line| line.trim().starts_with("a=max-fr:"))
    {
        lines[media_start + existing] = frame_rate_line;
        return lines.join("\n");
    }

    let mut insert_index = media_start + 1;
    while insert_index < media_end {
        let trimmed = lines[insert_index].trim();
        if trimmed.starts_with("c=") || trimmed.starts_with("i=") || trimmed.starts_with("b=") {
            insert_index += 1;
            continue;
        }
        break;
    }
    lines.insert(insert_index, frame_rate_line);
    lines.join("\n")
}

/// `overrideH264ProfileLevelId`: rewrites (or prepends) `profile-level-id` on
/// the fmtp line of the first H264 payload listed on the `m=video` line.
pub fn override_h264_profile_level_id(sdp: &str, profile_level_id: &str) -> String {
    let mut lines = split_lines(sdp);

    // First H264 payload in m=video order.
    let target_payload = lines
        .iter()
        .find(|line| line.trim().starts_with("m=video"))
        .and_then(|video_line| {
            let components: Vec<&str> = video_line.trim().split(' ').collect();
            if components.len() <= 3 {
                return None;
            }
            let pt_to_codec = video_payload_types(&lines);
            components[3..]
                .iter()
                .find(|pt| codec_for(&pt_to_codec, pt) == "H264")
                .map(|pt| (*pt).to_owned())
        });
    let Some(target_payload) = target_payload else {
        return sdp.to_owned();
    };

    let replacement = format!("profile-level-id={profile_level_id}");
    for line in &mut lines {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("a=fmtp:") else {
            continue;
        };
        let payload_type = first_section(rest, ' ');
        if payload_type != target_payload {
            continue;
        }

        let params_string = rest.split_once(' ').map_or("", |(_, params)| params);
        let mut params: Vec<String> = params_string
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect();

        match params
            .iter()
            .position(|param| param.to_lowercase().starts_with("profile-level-id="))
        {
            Some(index) => params[index] = replacement,
            None => params.insert(0, replacement),
        }

        *line = format!("a=fmtp:{payload_type} {}", params.join(";"));
        break;
    }

    lines.join("\n")
}

/// `enableOpusStereo`: appends `stereo=1` to every Opus fmtp line that lacks it.
pub fn enable_opus_stereo(sdp: &str) -> String {
    let mut lines = split_lines(sdp);
    let mut opus_payload_types = HashSet::new();
    for raw in &lines {
        let Some(rest) = raw.trim().strip_prefix("a=rtpmap:") else {
            continue;
        };
        let mut parts = rest.split(' ');
        let (Some(pt), Some(codec)) = (parts.next(), parts.next()) else {
            continue;
        };
        if first_section(codec, '/').to_uppercase() == "OPUS" {
            opus_payload_types.insert(pt.to_owned());
        }
    }
    if opus_payload_types.is_empty() {
        return sdp.to_owned();
    }

    for line in &mut lines {
        let Some(rest) = line.trim().strip_prefix("a=fmtp:") else {
            continue;
        };
        let parts: Vec<&str> = rest.split(' ').collect();
        let Some(&payload_type) = parts.first() else {
            continue;
        };
        if !opus_payload_types.contains(payload_type) || parts.len() < 2 {
            continue;
        }
        let parameters = parts[1..].join(" ");
        if !parameters.to_lowercase().contains("stereo=1") {
            let parameters = if parameters.is_empty() {
                "stereo=1".to_owned()
            } else {
                format!("{parameters};stereo=1")
            };
            *line = format!("a=fmtp:{payload_type} {parameters}");
        }
    }
    lines.join("\n")
}

/// `processLocalSdp`: applies every requested modification to the local offer.
pub fn process_local_sdp(sdp: &str, options: &ProcessOptions) -> String {
    let mut result = sdp.to_owned();

    let codec = options.preferred_video_codec.trim();
    if !codec.is_empty() && codec.to_uppercase() != "H264" {
        result = reorder_video_codecs(&result, codec);
    }

    if options.max_video_bitrate_kbps > 0 && options.inject_bitrate_lines {
        result = set_bitrate(&result, "video", options.max_video_bitrate_kbps);
    }
    if options.max_audio_bitrate_kbps > 0 && options.inject_bitrate_lines {
        result = set_bitrate(&result, "audio", options.max_audio_bitrate_kbps);
    }
    if options.max_video_frame_rate > 0 {
        result = set_video_max_frame_rate(&result, options.max_video_frame_rate);
    }
    if !options.h264_profile_level_id_override.is_empty() {
        result = override_h264_profile_level_id(&result, &options.h264_profile_level_id_override);
    }
    if !options.h264_profile_level_id.is_empty() {
        result = utils::set_h264_profile_level_id(&result, &options.h264_profile_level_id);
    }
    if options.scale_video_caps_to_1080 {
        result = utils::scale_video_caps_1080(&result);
    }
    if options.stereo_audio {
        result = enable_opus_stereo(&result);
    }

    result
}

/// `processLocalSdpWithHardcodedDefaults`: the deterministic profile used when
/// UI tuning should be ignored.
pub fn process_local_sdp_with_hardcoded_defaults(sdp: &str) -> String {
    let options = ProcessOptions {
        max_video_bitrate_kbps: 120_000,
        max_audio_bitrate_kbps: 48_000,
        stereo_audio: true,
        preferred_video_codec: "H264".to_owned(),
        max_video_frame_rate: 60,
        h264_profile_level_id_override: "640029".to_owned(),
        ..ProcessOptions::default()
    };
    process_local_sdp(sdp, &options)
}

/// `processRemoteSdp`: remote SDP is passed through unchanged.
pub fn process_remote_sdp(sdp: &str) -> String {
    sdp.to_owned()
}

/// `formatSdpForLogging`: normalises line endings, drops blank lines, puts an
/// empty line before each `m=` section and, when `redact_sensitive` is set,
/// hides ICE credentials, DTLS fingerprints and candidates.
pub fn format_sdp_for_logging(sdp: &str, redact_sensitive: bool) -> String {
    let mut output: Vec<String> = Vec::new();
    for raw_line in split_lines(sdp) {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("m=") && !output.is_empty() {
            output.push(String::new());
        }
        if redact_sensitive {
            if line.starts_with("a=ice-ufrag:") || line.starts_with("a=ice-pwd:") {
                output.push(format!("{}:<redacted>", first_section(line, ':')));
                continue;
            }
            if line.starts_with("a=fingerprint:") {
                output.push(format!("{} <redacted>", first_section(line, ' ')));
                continue;
            }
            if line.starts_with("a=candidate:") {
                output.push("a=candidate:<redacted>".to_owned());
                continue;
            }
        }
        output.push(line.to_owned());
    }
    output.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0
o=- 1 2 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 UDP/TLS/RTP/SAVPF 111
c=IN IP4 0.0.0.0
a=rtpmap:111 opus/48000/2
a=fmtp:111 minptime=10
m=video 9 UDP/TLS/RTP/SAVPF 96 97 98
c=IN IP4 0.0.0.0
a=rtpmap:96 H264/90000
a=fmtp:96 profile-level-id=42001f;packetization-mode=1
a=rtpmap:97 H264/90000
a=fmtp:97 profile-level-id=42e01f
a=rtpmap:98 VP9/90000
";

    #[test]
    fn h264_priority_ordering() {
        assert_eq!(h264_profile_priority("profile-level-id=4d001f"), 0);
        assert_eq!(h264_profile_priority("profile-level-id=42e01f"), 1);
        assert_eq!(h264_profile_priority("profile-level-id=42001f"), 2);
        assert_eq!(h264_profile_priority("profile-level-id=640029"), 99);
        assert_eq!(h264_profile_priority(""), 99);
    }

    fn sdp_processor_bitrate_helper(result: &str) {
        // b=AS is inserted after the media's c= line, matching sdp.ts.
        assert!(result.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111\nc=IN IP4 0.0.0.0\nb=AS:48000"));
        assert!(
            result.contains("m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\nc=IN IP4 0.0.0.0\nb=AS:120000")
        );
    }

    #[test]
    fn bitrate_injection() {
        let result = set_bitrate(OFFER, "video", 120000);
        sdp_processor_bitrate_helper(&set_bitrate(&result, "audio", 48000));
    }

    #[test]
    fn reorders_preferred_codec() {
        let result = reorder_video_codecs(OFFER, "VP9");
        let video_index = result.find("m=video");
        let vp9_index = result.find("a=rtpmap:98 VP9");
        assert!(video_index.is_some());
        assert!(vp9_index > video_index);
        assert!(result.contains("m=video 9 UDP/TLS/RTP/SAVPF 98 96 97"));
    }

    #[test]
    fn opus_stereo() {
        let result = enable_opus_stereo(OFFER);
        assert!(result.contains("a=fmtp:111 minptime=10;stereo=1"));
        // Applying twice must not duplicate the parameter.
        let twice = enable_opus_stereo(&result);
        assert_eq!(twice.matches("stereo=1").count(), 1);
    }

    #[test]
    fn max_frame_rate() {
        let result = set_video_max_frame_rate(OFFER, 60);
        assert!(result.contains("a=max-fr:60"));
        let replaced = set_video_max_frame_rate(&result, 30);
        assert!(replaced.contains("a=max-fr:30"));
        assert_eq!(replaced.matches("a=max-fr:").count(), 1);
    }

    #[test]
    fn h264_profile_override() {
        let result = override_h264_profile_level_id(OFFER, "640029");
        assert!(result.contains("a=fmtp:96 profile-level-id=640029;packetization-mode=1"));
        assert!(!result.contains("a=fmtp:96 profile-level-id=42001f"));
    }

    #[test]
    fn logging_redacts_secrets() {
        let sdp = "a=ice-ufrag:SECRET\na=ice-pwd:PASSWORD\na=candidate:1 1 UDP 1 1.2.3.4 9 \
                   typ host\na=fingerprint:sha-256 AA:BB";
        let formatted = format_sdp_for_logging(sdp, true);
        assert!(formatted.contains("a=ice-ufrag:<redacted>"));
        assert!(formatted.contains("a=ice-pwd:<redacted>"));
        assert!(formatted.contains("a=candidate:<redacted>"));
        assert!(!formatted.contains("SECRET"));
        assert!(!formatted.contains("PASSWORD"));
    }

    // Additional checks for the line-ending contract and the composed pipeline.

    #[test]
    fn modified_offers_are_lf_joined_and_untouched_offers_keep_crlf() {
        let crlf = OFFER.replace('\n', "\r\n");
        // A transform that changes the offer re-joins with LF.
        let stereo = enable_opus_stereo(&crlf);
        assert!(!stereo.contains('\r'));
        assert!(stereo.contains("a=fmtp:111 minptime=10;stereo=1\n"));
        // A transform with nothing to do returns the input verbatim.
        assert_eq!(reorder_video_codecs(&crlf, "AV1"), crlf);
        assert_eq!(set_bitrate(&crlf, "application", 100), crlf);
        assert_eq!(
            enable_opus_stereo("v=0\r\nm=video 9 X 96\r\n"),
            "v=0\r\nm=video 9 X 96\r\n"
        );
        assert_eq!(process_remote_sdp(&crlf), crlf);
    }

    #[test]
    fn hardcoded_defaults_apply_every_transform() {
        let result = process_local_sdp_with_hardcoded_defaults(OFFER);
        assert!(result.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111\nc=IN IP4 0.0.0.0\nb=AS:48000\n"));
        assert!(result.contains(
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\nc=IN IP4 0.0.0.0\nb=AS:120000\na=max-fr:60\n"
        ));
        assert!(result.contains("a=fmtp:96 profile-level-id=640029;packetization-mode=1\n"));
        assert!(result.contains("a=fmtp:97 profile-level-id=42e01f\n"));
        assert!(result.contains("a=fmtp:111 minptime=10;stereo=1\n"));
        // H264 is already first, so the m=video order is untouched.
        assert_eq!(
            result
                .matches("m=video 9 UDP/TLS/RTP/SAVPF 96 97 98")
                .count(),
            1
        );
    }

    #[test]
    fn process_local_sdp_honours_option_switches() {
        let options = ProcessOptions {
            max_video_bitrate_kbps: 5000,
            max_audio_bitrate_kbps: 64,
            inject_bitrate_lines: false,
            stereo_audio: false,
            preferred_video_codec: " vp9 ".to_owned(),
            scale_video_caps_to_1080: true,
            h264_profile_level_id: "42e020".to_owned(),
            ..ProcessOptions::default()
        };
        let sdp = format!("{OFFER}a=fmtp:99 max-fs=3600;max-mbps=108000\n");
        let result = process_local_sdp(&sdp, &options);
        assert!(!result.contains("b=AS:"));
        assert!(!result.contains("stereo=1"));
        assert!(result.contains("m=video 9 UDP/TLS/RTP/SAVPF 98 96 97"));
        assert!(result.contains("a=fmtp:96 profile-level-id=42e020;packetization-mode=1"));
        assert!(result.contains("a=fmtp:97 profile-level-id=42e01f"));
        assert!(result.contains("max-fs=8160;max-mbps=489600"));
    }

    // --- tst_datachannelconfig.cpp ------------------------------------------

    #[test]
    fn sdp_options_can_omit_bitrate_lines() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 102\nc=IN IP4 0.0.0.0\n";
        let options = ProcessOptions {
            max_video_bitrate_kbps: 120_000,
            inject_bitrate_lines: false,
            stereo_audio: false,
            ..ProcessOptions::default()
        };
        let result = process_local_sdp(sdp, &options);
        assert!(!result.contains("b=AS"));
    }

    #[test]
    fn sdp_options_scale_video_caps() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 102\n\
                   a=fmtp:102 packetization-mode=1;max-fs=3600;max-mbps=108000\n";
        let options = ProcessOptions {
            stereo_audio: false,
            scale_video_caps_to_1080: true,
            ..ProcessOptions::default()
        };
        let result = process_local_sdp(sdp, &options);
        assert!(result.contains("max-fs=8160"));
        assert!(result.contains("max-mbps=489600"));
    }

    #[test]
    fn logging_without_redaction_groups_sections() {
        let formatted =
            format_sdp_for_logging("v=0\r\n\r\nm=audio 9\r\n a=x \r\nm=video 9\n", false);
        assert_eq!(formatted, "v=0\n\nm=audio 9\na=x\n\nm=video 9");
        let redacted = format_sdp_for_logging("a=fingerprint:sha-256 AA:BB", true);
        assert_eq!(redacted, "a=fingerprint:sha-256 <redacted>");
    }
}
