//! Controller mapping probe: prints what the platform backend reads so a mis-mapped
//! button or trigger can be seen directly.
//!
//! `cargo run -p xcloud-input --example probe -- [seconds]`
//!
//! On Windows and Linux it prints the raw gilrs events next to the mapped
//! `RawGamepadState`. On macOS controllers come from GameController, which has no raw
//! event stream, so it prints the mapped state whenever it changes.

#[cfg(not(target_os = "macos"))]
mod gilrs_probe {
    use std::time::{Duration, Instant};

    use gilrs::{Axis, Button, Event, EventType};

    fn describe(id: gilrs::GamepadId, pad: &gilrs::Gamepad<'_>) {
        println!(
            "gamepad {id}: name={:?} uuid={} mapping_source={:?} os_name={:?} ff={}",
            pad.name(),
            pad.uuid()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            pad.mapping_source(),
            pad.os_name(),
            pad.is_ff_supported()
        );
        for b in [
            Button::South,
            Button::East,
            Button::West,
            Button::North,
            Button::LeftTrigger,
            Button::RightTrigger,
            Button::LeftTrigger2,
            Button::RightTrigger2,
            Button::Select,
            Button::Start,
            Button::Mode,
            Button::LeftThumb,
            Button::RightThumb,
            Button::DPadUp,
            Button::DPadDown,
            Button::DPadLeft,
            Button::DPadRight,
        ] {
            println!("  button {:?} -> code {:?}", b, pad.button_code(b));
        }
        for a in [
            Axis::LeftStickX,
            Axis::LeftStickY,
            Axis::LeftZ,
            Axis::RightStickX,
            Axis::RightStickY,
            Axis::RightZ,
            Axis::DPadX,
            Axis::DPadY,
        ] {
            println!("  axis {:?} -> code {:?}", a, pad.axis_code(a));
        }
    }

    pub fn main() {
        let seconds: u64 = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        // Raw events (no hat->button filter), the same configuration the sampler uses.
        let mut gilrs = match gilrs::GilrsBuilder::new()
            .with_default_filters(false)
            .build()
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("gilrs init failed: {e}");
                return;
            }
        };
        for (id, pad) in gilrs.gamepads() {
            describe(id, &pad);
        }
        println!("--- press LT, RT, LB, RB, D-pad one at a time ({seconds} s) ---");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(seconds) {
            while let Some(Event { id, event, .. }) = gilrs.next_event() {
                match event {
                    EventType::ButtonChanged(b, v, code) => {
                        println!(
                            "[{:6.2}s] ButtonChanged {:?} value={:.3} code={:?}",
                            start.elapsed().as_secs_f32(),
                            b,
                            v,
                            code
                        )
                    }
                    EventType::ButtonPressed(b, code) => println!(
                        "[{:6.2}s] ButtonPressed {:?} code={:?}",
                        start.elapsed().as_secs_f32(),
                        b,
                        code
                    ),
                    EventType::ButtonReleased(b, code) => println!(
                        "[{:6.2}s] ButtonReleased {:?} code={:?}",
                        start.elapsed().as_secs_f32(),
                        b,
                        code
                    ),
                    EventType::AxisChanged(a, v, code) => {
                        if v.abs() > 0.2 || matches!(a, Axis::DPadX | Axis::DPadY) {
                            println!(
                                "[{:6.2}s] AxisChanged {:?} value={:.3} code={:?}",
                                start.elapsed().as_secs_f32(),
                                a,
                                v,
                                code
                            )
                        }
                    }
                    EventType::Connected => {
                        println!("[{:6.2}s] Connected {id}", start.elapsed().as_secs_f32());
                        describe(id, &gilrs.gamepad(id));
                    }
                    EventType::Disconnected => {
                        println!("[{:6.2}s] Disconnected {id}", start.elapsed().as_secs_f32())
                    }
                    _ => {}
                }
                let pad = gilrs.gamepad(id);
                let state = xcloud_input::native::map_gamepad_debug(&pad);
                if let Some(line) = state {
                    println!("           mapped: {line}");
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    gilrs_probe::main();
}

/// GameController needs the main run loop, so the main thread spins it while a worker
/// samples like the stream's input thread does.
#[cfg(target_os = "macos")]
fn main() {
    use std::time::{Duration, Instant};

    use objc2_foundation::{NSDate, NSRunLoop};
    use xcloud_input::native::SharedGamepad;

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    println!("--- press buttons, pull triggers, move sticks ({seconds} s) ---");
    let worker = std::thread::spawn(move || {
        let pad = SharedGamepad::new();
        let start = Instant::now();
        let mut last = None;
        while start.elapsed() < Duration::from_secs(seconds) {
            let sample = pad.sample();
            if last.as_ref() != Some(&sample) {
                let at = start.elapsed().as_secs_f32();
                if sample.connected {
                    let s = &sample.state;
                    println!(
                        "[{at:6.2}s] {:?} buttons={:#06x} LT={:.3}{} RT={:.3}{} L=({:.2},{:.2}) R=({:.2},{:.2})",
                        sample.name,
                        s.buttons.bits(),
                        s.left_trigger,
                        if s.left_trigger_pressed { "*" } else { "" },
                        s.right_trigger,
                        if s.right_trigger_pressed { "*" } else { "" },
                        s.left_thumb.x,
                        s.left_thumb.y,
                        s.right_thumb.x,
                        s.right_thumb.y,
                    );
                } else {
                    println!("[{at:6.2}s] no controller");
                }
                last = Some(sample);
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    });
    while !worker.is_finished() {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
        NSRunLoop::mainRunLoop().runUntilDate(&until);
    }
    let _ = worker.join();
}
