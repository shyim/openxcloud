//! Decodes an Annex-B H.264 file through the VideoToolbox backend and prints decode
//! latency and throughput, once as fast as possible and once paced at 60 fps.
//!
//! ```text
//! cargo run --release -p xcloud-media --example decode_file [path/to/stream.h264]
//! ```
//!
//! Without an argument it decodes `fixtures/testsrc_1080p60.h264` (see fixtures/README.md).

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    macos::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("decode_file: the VideoToolbox backend only exists on macOS");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use xcloud_media::annexb::split_access_units;
    use xcloud_media::vt::{VtConfig, VtDecoder};
    use xcloud_media::{FrameSink, VideoDecoder};

    const DEFAULT_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/testsrc_1080p60.h264"
    );

    #[derive(Clone, Copy)]
    enum Pacing {
        Unpaced,
        Fps(u32),
    }

    struct Sample {
        decode: Duration,
        width: u32,
        height: u32,
    }

    struct RunStats {
        frames: usize,
        submitted: usize,
        dropped: u64,
        errors: u64,
        width: u32,
        height: u32,
        hardware: Option<bool>,
        wall: Duration,
        bytes: usize,
        latencies_ms: Vec<f64>,
    }

    fn run(aus: &[&[u8]], pacing: Pacing) -> Result<RunStats, Box<dyn std::error::Error>> {
        let samples: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::with_capacity(aus.len())));
        let sink_samples = Arc::clone(&samples);
        let sink: FrameSink = Box::new(move |frame| {
            if let Ok(mut v) = sink_samples.lock() {
                v.push(Sample {
                    decode: frame.decode_latency(),
                    width: frame.width,
                    height: frame.height,
                });
            }
        });
        let mut decoder = VtDecoder::new(VtConfig::default(), sink);

        let start = Instant::now();
        let mut submitted = 0usize;
        let mut bytes = 0usize;
        for (i, au) in aus.iter().enumerate() {
            if let Pacing::Fps(fps) = pacing {
                let due = start + Duration::from_secs_f64(i as f64 / f64::from(fps));
                pace_until(due);
            }
            match decoder.decode(au, Instant::now()) {
                Ok(()) => {
                    submitted += 1;
                    bytes += au.len();
                }
                Err(e) => eprintln!("AU {i}: {e}"),
            }
        }
        decoder.flush();
        let wall = start.elapsed();

        let samples = samples.lock().map_err(|_| "sample mutex poisoned")?;
        let mut latencies_ms: Vec<f64> = samples
            .iter()
            .map(|s| s.decode.as_secs_f64() * 1e3)
            .collect();
        latencies_ms.sort_by(f64::total_cmp);
        let (width, height) = samples
            .first()
            .map(|s| (s.width, s.height))
            .unwrap_or((0, 0));
        Ok(RunStats {
            frames: samples.len(),
            submitted,
            dropped: decoder.frames_dropped(),
            errors: decoder.frame_errors(),
            width,
            height,
            hardware: decoder.using_hardware(),
            wall,
            bytes,
            latencies_ms,
        })
    }

    /// Sleeps coarsely, then spins for the last stretch so the pacing is tight.
    fn pace_until(due: Instant) {
        loop {
            let now = Instant::now();
            if now >= due {
                return;
            }
            let remaining = due - now;
            if remaining > Duration::from_micros(1500) {
                std::thread::sleep(remaining - Duration::from_micros(1000));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let rank = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[rank.min(sorted.len() - 1)]
    }

    fn report(label: &str, s: &RunStats) {
        let avg = if s.latencies_ms.is_empty() {
            0.0
        } else {
            s.latencies_ms.iter().sum::<f64>() / s.latencies_ms.len() as f64
        };
        let max = s.latencies_ms.last().copied().unwrap_or(0.0);
        let secs = s.wall.as_secs_f64();
        println!("== {label}");
        println!(
            "   frames: {} decoded / {} submitted, dropped {}, callback errors {}",
            s.frames, s.submitted, s.dropped, s.errors
        );
        println!(
            "   resolution: {}x{}  hardware decoder: {}",
            s.width,
            s.height,
            s.hardware.map_or("unknown".to_string(), |b| b.to_string())
        );
        println!(
            "   submit→callback latency: avg {avg:.2} ms  p50 {:.2} ms  p99 {:.2} ms  max {max:.2} ms",
            percentile(&s.latencies_ms, 50.0),
            percentile(&s.latencies_ms, 99.0)
        );
        println!(
            "   wall {:.1} ms → {:.1} fps, {:.2} MB/s of bitstream",
            secs * 1e3,
            s.frames as f64 / secs,
            s.bytes as f64 / secs / 1e6
        );
    }

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::env::args()
            .nth(1)
            .unwrap_or_else(|| DEFAULT_FIXTURE.to_string());
        let stream = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let aus = split_access_units(&stream);
        println!(
            "{path}: {} bytes, {} access units, software env {}",
            stream.len(),
            aus.len(),
            if std::env::var_os(xcloud_media::vt::SOFTWARE_VIDEO_ENV).is_some() {
                "set"
            } else {
                "unset"
            }
        );
        if aus.is_empty() {
            return Err("no access units found".into());
        }

        // Warm up: the first session creation and the first keyframe are the slow part.
        let _ = run(&aus[..aus.len().min(8)], Pacing::Unpaced)?;

        let fast = run(&aus, Pacing::Unpaced)?;
        report("as fast as possible", &fast);
        let paced = run(&aus, Pacing::Fps(60))?;
        report("paced at 60 fps", &paced);
        Ok(())
    }
}
