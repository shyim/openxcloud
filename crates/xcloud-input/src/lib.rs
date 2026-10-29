//! Controller and keyboard input for the stream: samples a physical gamepad (gilrs) and a
//! keyboard virtual controller into the protocol's `InputQueue`. Ports
//! `qt/src/app/GamepadInputSource.{h,cpp}`, `NativeGamepad.{h,mm}` and the key table in
//! `qt/qml/StreamView.qml`.

pub mod keyboard;
pub mod native;
pub mod source;
pub mod system_gestures;
