//! Port of qt/src/streaming/OpusDecoder.{h,cpp}: libopus-backed decoder for the native
//! audio path. Decodes one Opus packet to interleaved 16-bit PCM, 48 kHz stereo by default.
//!
//! The decoder runs inline on the transport's pump thread (docs/archive/PORTING_PLAN.md section 4), so
//! nothing here allocates per packet beyond growing the caller's output buffer.

use opus::Channels;

/// Longest Opus frame: 120 ms. `opus_decode` needs room for it even when the packet turns
/// out to be shorter.
const MAX_FRAME_MS: u32 = 120;

/// Decodes Opus packets to interleaved `i16` PCM. Port of `OpusDecoder`.
///
/// Like the C++ class this never fails to construct: an unsupported configuration (or a
/// libopus failure) yields a decoder with [`is_valid`](Self::is_valid) `false` whose
/// `decode` returns 0. [`last_error`](Self::last_error) carries the libopus message.
pub struct OpusDecoder {
    inner: Option<opus::Decoder>,
    sample_rate: u32,
    channels: u16,
    last_error: Option<String>,
}

impl Default for OpusDecoder {
    /// 48 kHz stereo, what xCloud sends.
    fn default() -> Self {
        Self::new(48_000, 2)
    }
}

impl OpusDecoder {
    /// Creates a decoder for `sample_rate` Hz (8/12/16/24/48 kHz) and 1 or 2 channels.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let mut decoder = Self {
            inner: None,
            sample_rate,
            channels,
            last_error: None,
        };
        let layout = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => {
                decoder.last_error = Some(format!("unsupported channel count {other}"));
                return decoder;
            }
        };
        match opus::Decoder::new(sample_rate, layout) {
            Ok(inner) => decoder.inner = Some(inner),
            Err(err) => decoder.last_error = Some(err.to_string()),
        }
        decoder
    }

    /// `false` when libopus refused the configuration; `decode` then always returns 0.
    pub fn is_valid(&self) -> bool {
        self.inner.is_some()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The most recent libopus error message, cleared by a successful decode.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Decodes `packet`, appending interleaved `i16` PCM to `out`. Returns the number of
    /// samples per channel decoded, 0 on failure (nothing is appended then).
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> usize {
        self.decode_impl(packet, out, false)
    }

    /// Packet loss concealment: synthesises `samples_per_channel` samples (normally the
    /// duration of the missing packet, 960 for 20 ms at 48 kHz) from the decoder state and
    /// appends them to `out`. Returns the number of samples per channel produced.
    ///
    /// The C++ pipeline has no concealment (it simply skips lost packets); this is the
    /// `opus_decode(NULL)` path the Rust jitter buffer may use instead.
    pub fn conceal(&mut self, samples_per_channel: usize, out: &mut Vec<i16>) -> usize {
        let Some(inner) = self.inner.as_mut() else {
            return 0;
        };
        let channels = usize::from(self.channels);
        let offset = out.len();
        out.resize(offset + samples_per_channel * channels, 0);
        match inner.decode(&[], &mut out[offset..], false) {
            Ok(decoded) if decoded > 0 => {
                out.truncate(offset + decoded * channels);
                self.last_error = None;
                decoded
            }
            Ok(_) => {
                out.truncate(offset);
                0
            }
            Err(err) => {
                out.truncate(offset);
                self.last_error = Some(err.to_string());
                0
            }
        }
    }

    /// Resets the decoder state (`OPUS_RESET_STATE`), e.g. after a long gap.
    pub fn reset(&mut self) {
        if let Some(inner) = self.inner.as_mut()
            && let Err(err) = inner.reset_state()
        {
            self.last_error = Some(err.to_string());
        }
    }

    fn decode_impl(&mut self, packet: &[u8], out: &mut Vec<i16>, fec: bool) -> usize {
        let Some(inner) = self.inner.as_mut() else {
            return 0;
        };
        if packet.is_empty() {
            // An empty slice means "lost packet" to libopus; `conceal` is the explicit API.
            return 0;
        }
        let channels = usize::from(self.channels);
        let max_samples = (self.sample_rate * MAX_FRAME_MS / 1000) as usize;
        let offset = out.len();
        out.resize(offset + max_samples * channels, 0);
        match inner.decode(packet, &mut out[offset..], fec) {
            Ok(decoded) if decoded > 0 => {
                out.truncate(offset + decoded * channels);
                self.last_error = None;
                decoded
            }
            Ok(_) => {
                out.truncate(offset);
                0
            }
            Err(err) => {
                out.truncate(offset);
                self.last_error = Some(err.to_string());
                0
            }
        }
    }
}

impl std::fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("valid", &self.is_valid())
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("last_error", &self.last_error)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::{Application, Bitrate, Encoder};

    const SAMPLES_PER_CHANNEL: usize = 960; // 20 ms at 48 kHz

    /// Encodes one 20 ms stereo frame the way `tst_decoders.cpp` does with `opus_encode`
    /// (48 kHz, stereo, VoIP, 32 kbit/s).
    fn encode_frame(pcm: &[i16]) -> Vec<u8> {
        let mut encoder = Encoder::new(48_000, Channels::Stereo, Application::Voip).unwrap();
        encoder.set_bitrate(Bitrate::Bits(32_000)).unwrap();
        let packet = encoder.encode_vec(pcm, 4000).unwrap();
        assert!(!packet.is_empty());
        packet
    }

    /// 440 Hz stereo tone, 20 ms, amplitude 0.5.
    fn test_tone() -> Vec<i16> {
        (0..SAMPLES_PER_CHANNEL)
            .flat_map(|n| {
                let t = n as f32 / 48_000.0;
                let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 16_000.0) as i16;
                [v, v]
            })
            .collect()
    }

    /// Port of `TestDecoders::opusDecodesEncodedPacket`: 20 ms of silence, stereo 16-bit.
    #[test]
    fn opus_decodes_encoded_packet() {
        let pcm = vec![0i16; SAMPLES_PER_CHANNEL * 2];
        let packet = encode_frame(&pcm);

        let mut decoder = OpusDecoder::default();
        assert!(decoder.is_valid());
        let mut out = Vec::new();
        let decoded = decoder.decode(&packet, &mut out);
        assert_eq!(decoded, SAMPLES_PER_CHANNEL);
        assert_eq!(out.len(), SAMPLES_PER_CHANNEL * 2);
    }

    #[test]
    fn opus_decodes_test_tone_to_non_silence() {
        let packet = encode_frame(&test_tone());
        let mut decoder = OpusDecoder::default();
        let mut out = Vec::new();
        assert_eq!(decoder.decode(&packet, &mut out), SAMPLES_PER_CHANNEL);
        assert_eq!(out.len(), SAMPLES_PER_CHANNEL * 2);
        let peak = out.iter().map(|s| i32::from(*s).abs()).max().unwrap();
        assert!(peak > 1000, "decoded tone is silent (peak {peak})");
        assert!(decoder.last_error().is_none());
    }

    #[test]
    fn decode_appends_after_existing_samples() {
        let packet = encode_frame(&test_tone());
        let mut decoder = OpusDecoder::default();
        let mut out = vec![7i16; 5];
        assert_eq!(decoder.decode(&packet, &mut out), SAMPLES_PER_CHANNEL);
        assert_eq!(out.len(), 5 + SAMPLES_PER_CHANNEL * 2);
        assert_eq!(&out[..5], &[7, 7, 7, 7, 7]);
    }

    #[test]
    fn invalid_packet_returns_zero_and_leaves_buffer_untouched() {
        let mut decoder = OpusDecoder::default();
        let mut out = vec![1i16, 2, 3];
        // TOC code 3 with a frame count of 0 is an invalid packet.
        assert_eq!(decoder.decode(&[0x03, 0x00], &mut out), 0);
        assert_eq!(out, vec![1, 2, 3]);
        assert!(decoder.last_error().is_some());
        // A later good packet clears the error.
        let packet = encode_frame(&test_tone());
        assert_eq!(decoder.decode(&packet, &mut out), SAMPLES_PER_CHANNEL);
        assert!(decoder.last_error().is_none());
    }

    #[test]
    fn empty_packet_decodes_nothing() {
        let mut decoder = OpusDecoder::default();
        let mut out = Vec::new();
        assert_eq!(decoder.decode(&[], &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn conceal_fills_the_lost_frame() {
        let packet = encode_frame(&test_tone());
        let mut decoder = OpusDecoder::default();
        let mut out = Vec::new();
        decoder.decode(&packet, &mut out);
        out.clear();
        assert_eq!(
            decoder.conceal(SAMPLES_PER_CHANNEL, &mut out),
            SAMPLES_PER_CHANNEL
        );
        assert_eq!(out.len(), SAMPLES_PER_CHANNEL * 2);
    }

    #[test]
    fn reset_keeps_the_decoder_usable() {
        let packet = encode_frame(&test_tone());
        let mut decoder = OpusDecoder::default();
        let mut out = Vec::new();
        decoder.decode(&packet, &mut out);
        decoder.reset();
        assert!(decoder.is_valid());
        assert!(decoder.last_error().is_none());
        out.clear();
        assert_eq!(decoder.decode(&packet, &mut out), SAMPLES_PER_CHANNEL);
    }

    #[test]
    fn unsupported_configuration_is_invalid() {
        let three = OpusDecoder::new(48_000, 3);
        assert!(!three.is_valid());
        assert_eq!(three.channels(), 3);
        assert!(three.last_error().unwrap().contains("channel"));
        let mut out = Vec::new();
        assert_eq!(
            OpusDecoder::new(48_000, 3).decode(&[0xfc, 0xff, 0xfe], &mut out),
            0
        );
        assert!(out.is_empty());

        let odd_rate = OpusDecoder::new(44_100, 2);
        assert!(!odd_rate.is_valid());
        assert!(odd_rate.last_error().is_some());
    }

    #[test]
    fn mono_decoder_reports_its_layout() {
        let mono = OpusDecoder::new(16_000, 1);
        assert!(mono.is_valid());
        assert_eq!(mono.sample_rate(), 16_000);
        assert_eq!(mono.channels(), 1);
        // Stereo packet decoded by a mono decoder: libopus downmixes, 20 ms at 16 kHz.
        let packet = encode_frame(&test_tone());
        let mut mono = mono;
        let mut out = Vec::new();
        assert_eq!(mono.decode(&packet, &mut out), 320);
        assert_eq!(out.len(), 320);
    }
}
