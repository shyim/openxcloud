//! Annex-B helpers for the access units `VideoJitterBuffer` assembles: NAL unit iteration
//! over 3- and 4-byte start codes, IDR detection, SPS/PPS extraction, SPS parsing and the
//! AVCC (4-byte big-endian length prefixed) conversion VideoToolbox wants.
//!
//! This replaces the `av_parser_parse2` layer of `qt/src/streaming/H264Decoder.cpp`: the
//! jitter buffer already delivers whole access units, so we only need to re-frame them.

use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::rbsp::{BitReader, ByteReader};

use crate::{Error, Result};

/// `nal_unit_type` 1: coded slice of a non-IDR picture.
pub const NAL_SLICE: u8 = 1;
/// `nal_unit_type` 5: coded slice of an IDR picture.
pub const NAL_IDR: u8 = 5;
/// `nal_unit_type` 6: SEI.
pub const NAL_SEI: u8 = 6;
/// `nal_unit_type` 7: sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// `nal_unit_type` 8: picture parameter set.
pub const NAL_PPS: u8 = 8;
/// `nal_unit_type` 9: access unit delimiter.
pub const NAL_AUD: u8 = 9;

/// Position of a NAL unit inside an Annex-B byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NalSpan {
    /// First byte of the start code (`00 00 01` or `00 00 00 01`).
    start_code: usize,
    /// First byte of the NAL unit (its header byte).
    payload: usize,
    /// One past the last payload byte, trailing zero bytes removed.
    end: usize,
}

/// Finds the next `00 00 01` at or after `from`. Returns the index of the first zero of
/// the three-byte sequence.
fn find_three_byte_start_code(data: &[u8], from: usize) -> Option<usize> {
    if data.len() < 3 {
        return None;
    }
    (from..=data.len() - 3).find(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
}

/// Yields the spans of all NAL units in `data`, in order.
fn nal_spans(data: &[u8]) -> impl Iterator<Item = NalSpan> + '_ {
    let mut cursor = find_three_byte_start_code(data, 0);
    std::iter::from_fn(move || {
        let sc = cursor?;
        let payload = sc + 3;
        let next = find_three_byte_start_code(data, payload);
        let mut end = next.unwrap_or(data.len());
        // trailing_zero_8bits and the leading zero of a 4-byte start code belong to
        // nobody: strip them from this NAL.
        while end > payload && data[end - 1] == 0 {
            end -= 1;
        }
        // A 4-byte start code starts one byte earlier.
        let start_code = if sc > 0 && data[sc - 1] == 0 {
            sc - 1
        } else {
            sc
        };
        cursor = next;
        Some(NalSpan {
            start_code,
            payload,
            end,
        })
    })
}

/// Iterates over the NAL units of an Annex-B access unit. Each item is the NAL unit
/// without its start code, starting with the header byte; trailing zero bytes are
/// stripped. Bytes before the first start code are ignored.
pub fn nal_units(au: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    nal_spans(au)
        .filter(|s| s.end > s.payload)
        .map(|s| &au[s.payload..s.end])
}

/// The `nal_unit_type` (low five bits of the header byte) of a NAL unit, if non-empty.
pub fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|header| header & 0x1f)
}

/// `true` for VCL NAL unit types (coded slices, 1–5).
pub fn is_vcl(nal_unit_type: u8) -> bool {
    (1..=5).contains(&nal_unit_type)
}

/// `true` if the access unit contains an IDR slice (a keyframe).
pub fn is_idr(au: &[u8]) -> bool {
    nal_units(au).any(|nal| nal_unit_type(nal) == Some(NAL_IDR))
}

/// Returns the last SPS and PPS NAL units found in `au` (without start codes).
pub fn find_parameter_sets(au: &[u8]) -> (Option<&[u8]>, Option<&[u8]>) {
    let mut sps = None;
    let mut pps = None;
    for nal in nal_units(au) {
        match nal_unit_type(nal) {
            Some(NAL_SPS) => sps = Some(nal),
            Some(NAL_PPS) => pps = Some(nal),
            _ => {}
        }
    }
    (sps, pps)
}

/// The active SPS and PPS of a stream, as raw NAL units including the header byte and
/// any emulation-prevention bytes (the form `CMVideoFormatDescriptionCreateFromH264ParameterSets`
/// expects).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl ParameterSets {
    /// Both parameter sets have been seen.
    pub fn is_complete(&self) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty()
    }

    /// Replaces the stored SPS/PPS with any found in `au`. Returns `true` if either one
    /// changed, which means the decoder's format description must be rebuilt.
    pub fn update_from(&mut self, au: &[u8]) -> bool {
        let (sps, pps) = find_parameter_sets(au);
        let mut changed = false;
        if let Some(sps) = sps
            && sps != self.sps.as_slice()
        {
            self.sps = sps.to_vec();
            changed = true;
        }
        if let Some(pps) = pps
            && pps != self.pps.as_slice()
        {
            self.pps = pps.to_vec();
            changed = true;
        }
        changed
    }
}

/// Converts an Annex-B access unit to AVCC framing: every NAL unit is prefixed with its
/// length as a 4-byte big-endian integer, start codes and trailing zeros are dropped.
pub fn annexb_to_avcc(au: &[u8]) -> Vec<u8> {
    annexb_to_avcc_filtered(au, |_| true)
}

/// Like [`annexb_to_avcc`], but keeps only NAL units whose type satisfies `keep`. Used to
/// drop SPS/PPS/AUD from the sample data handed to VideoToolbox (they live in the format
/// description instead).
pub fn annexb_to_avcc_filtered(au: &[u8], keep: impl Fn(u8) -> bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    for nal in nal_units(au) {
        let Some(kind) = nal_unit_type(nal) else {
            continue;
        };
        if !keep(kind) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// What the decoder needs to know from an SPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsInfo {
    /// Display width after frame cropping.
    pub width: u32,
    /// Display height after frame cropping.
    pub height: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
}

/// Parses an SPS NAL unit (with header byte and emulation-prevention bytes) with
/// `h264-reader` and returns its display dimensions.
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo> {
    if nal_unit_type(nal) != Some(NAL_SPS) {
        return Err(Error::InvalidSps(format!(
            "nal_unit_type {:?} is not an SPS",
            nal_unit_type(nal)
        )));
    }
    let sps = SeqParameterSet::from_bits(BitReader::new(ByteReader::skipping_h264_header(nal)))
        .map_err(|e| Error::InvalidSps(format!("{e:?}")))?;
    let (width, height) = sps
        .pixel_dimensions()
        .map_err(|e| Error::InvalidSps(format!("{e:?}")))?;
    Ok(SpsInfo {
        width,
        height,
        profile_idc: sps.profile_idc.into(),
        level_idc: sps.level_idc,
    })
}

/// `true` if this VCL NAL unit starts a new primary coded picture, i.e. its slice header
/// has `first_mb_in_slice == 0`. `first_mb_in_slice` is the first `ue(v)` field after the
/// NAL header; it is zero exactly when its first bit is `1`.
fn is_first_slice_of_picture(nal: &[u8]) -> bool {
    nal.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// Splits a raw Annex-B byte stream (a file, or a replayed dump) into access units.
///
/// A new access unit starts at an AUD, at an SPS/PPS/SEI that follows a slice, or at a
/// slice with `first_mb_in_slice == 0` that follows another slice (multi-slice pictures,
/// as produced by `x264 --tune zerolatency`). Each returned slice includes its start codes
/// so it can be fed to a decoder unchanged. The live path does not need this, the jitter
/// buffer already frames access units.
pub fn split_access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut au_start: Option<usize> = None;
    let mut seen_vcl = false;

    for span in nal_spans(stream) {
        let nal = &stream[span.payload..span.end];
        let Some(kind) = nal_unit_type(nal) else {
            continue;
        };
        let starts_new_au = seen_vcl
            && (kind == NAL_AUD
                || (!is_vcl(kind) && kind != NAL_AUD && matches!(kind, 6..=18))
                || (is_vcl(kind) && is_first_slice_of_picture(nal)));
        if starts_new_au && let Some(start) = au_start {
            units.push(&stream[start..span.start_code]);
            au_start = None;
            seen_vcl = false;
        }
        if au_start.is_none() {
            au_start = Some(span.start_code);
        }
        if is_vcl(kind) {
            seen_vcl = true;
        }
    }
    if let Some(start) = au_start {
        units.push(&stream[start..]);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS of `fixtures/testsrc_1080p60.h264` (x264, High 4.2, 1920x1080 with cropping).
    const SPS_1080P: &[u8] = &[
        0x67, 0x64, 0x00, 0x2a, 0xac, 0xb4, 0x03, 0xc0, 0x11, 0x3f, 0x2e, 0x02, 0x20, 0x00, 0x00,
        0x03, 0x00, 0x20, 0x00, 0x00, 0x0f, 0x11, 0xe3, 0x06, 0x54,
    ];
    /// PPS of `fixtures/testsrc_1080p60.h264`.
    const PPS_1080P: &[u8] = &[0x68, 0xef, 0x3c, 0xb0];
    /// SPS of `ffmpeg -f lavfi -i testsrc=size=64x64:rate=10 -c:v libx264 -preset ultrafast`
    /// (High 4:4:4 Predictive, 64x64).
    const SPS_64: &[u8] = &[
        0x67, 0xf4, 0x00, 0x0a, 0x91, 0x96, 0x84, 0x26, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04,
        0x00, 0x00, 0x03, 0x00, 0x52, 0x3c, 0x48, 0x9a, 0x80,
    ];

    #[test]
    fn nal_units_splits_three_and_four_byte_start_codes() {
        let au: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, // SPS, 4-byte start code
            0x00, 0x00, 0x01, 0x68, 0xBB, // PPS, 3-byte start code
            0x00, 0x00, 0x00, 0x01, 0x65, 0xCC, 0xDD, // IDR, 4-byte start code
        ];
        let nals: Vec<&[u8]> = nal_units(au).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 0xAA][..], &[0x68, 0xBB], &[0x65, 0xCC, 0xDD]]
        );
    }

    #[test]
    fn nal_units_strips_trailing_zero_bytes_and_leading_garbage() {
        let au: &[u8] = &[
            0x55, 0x66, // garbage before the first start code
            0x00, 0x00, 0x01, 0x41, 0x01, 0x00, 0x00, // slice followed by trailing_zero_8bits
            0x00, 0x00, 0x01, 0x41, 0x02,
        ];
        let nals: Vec<&[u8]> = nal_units(au).collect();
        assert_eq!(nals, vec![&[0x41, 0x01][..], &[0x41, 0x02]]);
    }

    #[test]
    fn nal_units_handles_empty_and_degenerate_input() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0x00, 0x00]).count(), 0);
        assert_eq!(nal_units(&[0x00, 0x00, 0x01]).count(), 0);
        // Two consecutive start codes: the empty NAL between them is skipped.
        let nals: Vec<&[u8]> = nal_units(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x41]).collect();
        assert_eq!(nals, vec![&[0x41][..]]);
    }

    #[test]
    fn nal_unit_type_reads_low_five_bits() {
        assert_eq!(nal_unit_type(&[0x67]), Some(NAL_SPS));
        assert_eq!(nal_unit_type(&[0x68]), Some(NAL_PPS));
        assert_eq!(nal_unit_type(&[0x65]), Some(NAL_IDR));
        assert_eq!(nal_unit_type(&[0x41]), Some(NAL_SLICE));
        assert_eq!(nal_unit_type(&[0x06]), Some(NAL_SEI));
        assert_eq!(nal_unit_type(&[0x09]), Some(NAL_AUD));
        assert_eq!(nal_unit_type(&[]), None);
        assert!(is_vcl(NAL_SLICE) && is_vcl(NAL_IDR));
        assert!(!is_vcl(NAL_SPS) && !is_vcl(NAL_SEI));
    }

    #[test]
    fn is_idr_detects_keyframes() {
        let idr: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0x00, 0x00, 0x01, 0x65, 0x88];
        let p: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x41, 0x9a];
        assert!(is_idr(idr));
        assert!(!is_idr(p));
        assert!(!is_idr(&[]));
    }

    #[test]
    fn find_parameter_sets_returns_last_sps_and_pps() {
        let au: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x01, // SPS #1
            0x00, 0x00, 0x00, 0x01, 0x67, 0x02, // SPS #2 (wins)
            0x00, 0x00, 0x01, 0x68, 0x03, // PPS
            0x00, 0x00, 0x01, 0x65, 0x04, // IDR
        ];
        let (sps, pps) = find_parameter_sets(au);
        assert_eq!(sps, Some(&[0x67, 0x02][..]));
        assert_eq!(pps, Some(&[0x68, 0x03][..]));

        let none: &[u8] = &[0x00, 0x00, 0x01, 0x41, 0x04];
        assert_eq!(find_parameter_sets(none), (None, None));
    }

    #[test]
    fn parameter_sets_update_reports_changes() {
        let mut ps = ParameterSets::default();
        assert!(!ps.is_complete());
        let keyframe: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x01, 0x00, 0x00, 0x01, 0x68, 0x02, 0x00, 0x00, 0x01,
            0x65, 0x03,
        ];
        assert!(ps.update_from(keyframe));
        assert!(ps.is_complete());
        assert_eq!(ps.sps, vec![0x67, 0x01]);
        assert_eq!(ps.pps, vec![0x68, 0x02]);
        // Same parameter sets again: no change.
        assert!(!ps.update_from(keyframe));
        // A P-frame carries none: no change.
        assert!(!ps.update_from(&[0x00, 0x00, 0x01, 0x41, 0x03]));
        // A new PPS only: change.
        assert!(ps.update_from(&[0x00, 0x00, 0x01, 0x68, 0x09]));
        assert_eq!(ps.pps, vec![0x68, 0x09]);
        assert_eq!(ps.sps, vec![0x67, 0x01]);
    }

    #[test]
    fn annexb_to_avcc_prefixes_big_endian_lengths() {
        let au: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, 0x03, 0x00, 0x00, 0x01, 0x41, 0xAA, 0x00,
        ];
        assert_eq!(
            annexb_to_avcc(au),
            vec![
                0x00, 0x00, 0x00, 0x04, 0x65, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x02, 0x41, 0xAA
            ]
        );
        assert!(annexb_to_avcc(&[]).is_empty());
    }

    #[test]
    fn annexb_to_avcc_handles_large_nal() {
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        au.extend(std::iter::repeat_n(0x5a, 70_000));
        let avcc = annexb_to_avcc(&au);
        assert_eq!(&avcc[..4], &70_001u32.to_be_bytes());
        assert_eq!(avcc.len(), 4 + 70_001);
    }

    #[test]
    fn annexb_to_avcc_filtered_drops_parameter_sets() {
        let au: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x01, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0x02, // PPS
            0x00, 0x00, 0x00, 0x01, 0x09, 0xf0, // AUD
            0x00, 0x00, 0x00, 0x01, 0x06, 0x05, // SEI
            0x00, 0x00, 0x00, 0x01, 0x65, 0x03, // IDR
        ];
        let vcl_and_sei =
            annexb_to_avcc_filtered(au, |t| !matches!(t, NAL_SPS | NAL_PPS | NAL_AUD));
        assert_eq!(
            vcl_and_sei,
            vec![
                0x00, 0x00, 0x00, 0x02, 0x06, 0x05, 0x00, 0x00, 0x00, 0x02, 0x65, 0x03
            ]
        );
    }

    #[test]
    fn parse_sps_reads_1080p_dimensions_with_cropping() {
        let info = parse_sps(SPS_1080P).expect("valid SPS");
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.profile_idc, 100);
        assert_eq!(info.level_idc, 42);
    }

    #[test]
    fn parse_sps_reads_64x64_high444() {
        let info = parse_sps(SPS_64).expect("valid SPS");
        assert_eq!((info.width, info.height), (64, 64));
        assert_eq!(info.profile_idc, 244);
        assert_eq!(info.level_idc, 10);
    }

    #[test]
    fn parse_sps_rejects_non_sps_and_garbage() {
        assert!(matches!(parse_sps(PPS_1080P), Err(Error::InvalidSps(_))));
        assert!(matches!(parse_sps(&[]), Err(Error::InvalidSps(_))));
        assert!(matches!(parse_sps(&[0x67]), Err(Error::InvalidSps(_))));
        assert!(matches!(
            parse_sps(&[0x67, 0xff, 0xff, 0xff]),
            Err(Error::InvalidSps(_))
        ));
    }

    #[test]
    fn split_access_units_groups_multi_slice_pictures() {
        // SPS, PPS, IDR slice 0, IDR slice 1 (first_mb != 0), P slice 0, P slice 1.
        let stream: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x01, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0x02, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, // IDR first_mb_in_slice=0 (first bit 1)
            0x00, 0x00, 0x01, 0x65, 0x20, 0x01, // IDR first_mb_in_slice!=0 (first bit 0)
            0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, // P first slice
            0x00, 0x00, 0x01, 0x41, 0x40, 0x02, // P second slice
        ];
        let aus = split_access_units(stream);
        assert_eq!(aus.len(), 2);
        assert_eq!(aus[0], &stream[..25]);
        assert_eq!(aus[1], &stream[25..]);
        assert!(is_idr(aus[0]));
        assert!(!is_idr(aus[1]));
        assert_eq!(nal_units(aus[0]).count(), 4);
        assert_eq!(nal_units(aus[1]).count(), 2);
    }

    #[test]
    fn split_access_units_honours_access_unit_delimiters_and_sei() {
        let stream: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x09, 0xf0, // AUD
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, // IDR
            0x00, 0x00, 0x00, 0x01, 0x09, 0xf0, // AUD → new AU
            0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, // P
            0x00, 0x00, 0x00, 0x01, 0x06, 0x05, 0x00, 0x80, // SEI → new AU
            0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, // P
        ];
        let aus = split_access_units(stream);
        assert_eq!(aus.len(), 3);
        assert_eq!(aus[0], &stream[..12]);
        assert_eq!(aus[1], &stream[12..24]);
        assert_eq!(aus[2], &stream[24..]);
    }

    #[test]
    fn split_access_units_on_empty_or_single_au() {
        assert!(split_access_units(&[]).is_empty());
        let single: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88];
        assert_eq!(split_access_units(single), vec![single]);
    }

    #[test]
    fn split_access_units_on_fixture_yields_120_frames() {
        let stream = include_bytes!("../../../fixtures/testsrc_1080p60.h264");
        let aus = split_access_units(stream);
        assert_eq!(aus.len(), 120);
        assert!(is_idr(aus[0]));
        assert!(is_idr(aus[60]), "GOP of 60 means frame 60 is an IDR");
        assert!(!is_idr(aus[1]));
        let (sps, pps) = find_parameter_sets(aus[0]);
        let info = parse_sps(sps.expect("fixture SPS")).unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert!(pps.is_some(), "fixture PPS");
        // Every byte of the stream is accounted for exactly once.
        assert_eq!(aus.iter().map(|a| a.len()).sum::<usize>(), stream.len());
    }
}
