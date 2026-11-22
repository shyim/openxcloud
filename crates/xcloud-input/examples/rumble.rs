//! Rumble check: plays one vibration report on the mapped controller and prints what
//! the backend answered.
//!
//! `cargo run -p xcloud-input --example rumble -- [strength 0..1] [milliseconds]`
//!
//! On macOS controllers come from GameController, which needs the main run loop, so the
//! main thread spins it while a worker thread samples and rumbles like the stream does.

use std::time::{Duration, Instant};

use xcloud_input::native::SharedGamepad;
use xcloud_proto::input::packet::VibrationReport;

/// Lets the platform deliver controller events for `duration`.
fn pump(duration: Duration) {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSDate, NSRunLoop};
        let until = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
        NSRunLoop::mainRunLoop().runUntilDate(&until);
    }
    #[cfg(not(target_os = "macos"))]
    std::thread::sleep(duration);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let strength: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.6);
    let millis: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(600);

    let pad = SharedGamepad::new();
    let worker = std::thread::spawn(move || {
        // Wait for a controller to be listed.
        let start = Instant::now();
        let name = loop {
            let sample = pad.sample();
            if sample.connected {
                break sample.name;
            }
            if start.elapsed() > Duration::from_secs(5) {
                println!("no controller found");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        println!("controller: {name}");
        let report = VibrationReport {
            left_motor_percent: strength,
            right_motor_percent: strength,
            left_trigger_motor_percent: strength,
            right_trigger_motor_percent: strength,
            duration_ms: millis,
            ..VibrationReport::default()
        };
        match pad.set_vibration(&report, 1.0) {
            Ok(true) => println!("rumble started ({strength} for {millis} ms)"),
            Ok(false) => println!("this controller cannot rumble"),
            Err(error) => println!("rumble failed: {error}"),
        }
        // The report's timeline is stepped by sampling, as on the stream's input thread.
        let end = Instant::now() + Duration::from_millis(u64::from(millis) + 300);
        while Instant::now() < end {
            pad.sample();
            std::thread::sleep(Duration::from_millis(8));
        }
        pad.stop_vibration();
        println!("done");
    });
    while !worker.is_finished() {
        pump(Duration::from_millis(50));
    }
    let _ = worker.join();
}
