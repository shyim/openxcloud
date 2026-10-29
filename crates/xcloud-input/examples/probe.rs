//! Controller mapping probe: prints raw gilrs events and the mapped `RawGamepadState`
//! so a mis-mapped button or trigger can be seen directly.
//!
//! `cargo run -p xcloud-input --example probe -- [seconds]`

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

fn main() {
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
