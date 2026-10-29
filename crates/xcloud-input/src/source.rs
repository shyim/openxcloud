//! Port of qt/src/app/GamepadInputSource.{h,cpp} and qt/src/app/NativeGamepad.h.
//!
//! Samples one physical controller (through a [`Sampler`]) and the keyboard virtual
//! controller into the stream's [`InputQueue`]. Differences from the Qt class:
//!
//! * No `QTimer`: the host calls [`GamepadInputSource::tick`] every
//!   [`POLL_INTERVAL_MS`] milliseconds while the source is active.
//! * Qt signals become return values: `connectedChanged` is reported through
//!   [`TickReport::connection_changed`]; the property-change signals are dropped
//!   (the getters are cheap to poll).
//! * The `InputQueue*` becomes a shared `Arc<Mutex<InputQueue>>` so the input channel
//!   can flush the same queue the source fills, exactly as the C++ shares the pointer.
//! * An optional [`ChordRecognizer`] is run over every produced frame; the Qt class did
//!   not wire it, so with no definitions (the default) behaviour is identical.

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::info;
use xcloud_proto::input::gamepad::{
    ChordDefinition, ChordRecognizer, GamepadHandler, RawGamepadState, ShortcutAction,
};
use xcloud_proto::input::models::{ControllerSettings, GamepadButtons, Vec2};
use xcloud_proto::input::queue::InputQueue;

/// The `QTimer` interval of the Qt source (`Qt::PreciseTimer`, 8 ms = 125 Hz).
pub const POLL_INTERVAL_MS: u64 = 8;

/// Every how many samples the C++ logged its sample counters.
const SAMPLE_LOG_INTERVAL: u64 = 625;

/// One poll of the physical controller. Port of `NativeGamepadSample`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NativeGamepadSample {
    pub connected: bool,
    pub name: String,
    pub state: RawGamepadState,
}

/// Port of `GamepadInputSource::Sampler` (`std::function<NativeGamepadSample()>`).
pub type Sampler = Box<dyn FnMut() -> NativeGamepadSample + Send>;

/// Shared handle to the queue the input channel flushes.
pub type SharedInputQueue = Arc<Mutex<InputQueue>>;

/// What one [`GamepadInputSource::tick`] changed, replacing the Qt signals.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TickReport {
    /// `connectedChanged`: the controller connected, disconnected or changed name.
    pub connection_changed: bool,
    /// Chord shortcuts that fired on this sample (empty without chord definitions).
    pub shortcuts: Vec<ShortcutAction>,
}

/// Maps the `setKey` names to protocol buttons. Port of the anonymous `buttonMap()`.
/// The trigger names (`left-trigger` / `right-trigger`) are handled by `set_key` itself.
pub fn button_from_name(name: &str) -> Option<GamepadButtons> {
    Some(match name {
        "a" => GamepadButtons::A,
        "b" => GamepadButtons::B,
        "x" => GamepadButtons::X,
        "y" => GamepadButtons::Y,
        "menu" => GamepadButtons::MENU,
        "view" => GamepadButtons::VIEW,
        "nexus" => GamepadButtons::NEXUS,
        "dpad-up" => GamepadButtons::DPAD_UP,
        "dpad-down" => GamepadButtons::DPAD_DOWN,
        "dpad-left" => GamepadButtons::DPAD_LEFT,
        "dpad-right" => GamepadButtons::DPAD_RIGHT,
        "left-shoulder" => GamepadButtons::LEFT_SHOULDER,
        "right-shoulder" => GamepadButtons::RIGHT_SHOULDER,
        "left-thumb" => GamepadButtons::LEFT_THUMB,
        "right-thumb" => GamepadButtons::RIGHT_THUMB,
        _ => return None,
    })
}

/// Trigger side for [`GamepadInputSource::set_trigger`] (the C++ `"left"` / `"right"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSide {
    Left,
    Right,
}

impl TriggerSide {
    /// Parses the C++ side name; anything else is ignored like the C++ did.
    pub fn from_name(side: &str) -> Option<Self> {
        match side {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// Samples a physical controller and keyboard fallback into the stream input queue.
pub struct GamepadInputSource {
    sampler: Sampler,
    controller_name: String,
    focused: bool,
    samples: u64,
    pressed_samples: u64,
    queue: Option<SharedInputQueue>,
    handler: GamepadHandler,
    chords: ChordRecognizer,
    settings: ControllerSettings,
    state: RawGamepadState,
    pressed: HashSet<&'static str>,
    active: bool,
    connected: bool,
}

impl std::fmt::Debug for GamepadInputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GamepadInputSource")
            .field("controller_name", &self.controller_name)
            .field("focused", &self.focused)
            .field("active", &self.active)
            .field("connected", &self.connected)
            .field("settings", &self.settings)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl GamepadInputSource {
    /// `GamepadInputSource(parent, sampler)`; the handler uses gamepad index 0.
    pub fn new(sampler: Sampler) -> Self {
        Self {
            sampler,
            controller_name: String::new(),
            focused: true,
            samples: 0,
            pressed_samples: 0,
            queue: None,
            handler: GamepadHandler::new(0),
            chords: ChordRecognizer::default(),
            settings: ControllerSettings::default(),
            state: RawGamepadState::default(),
            pressed: HashSet::new(),
            active: false,
            connected: false,
        }
    }

    /// `setInputQueue`: the queue frames are enqueued into; `None` detaches.
    pub fn set_input_queue(&mut self, queue: Option<SharedInputQueue>) {
        self.queue = queue;
    }

    pub fn input_queue(&self) -> Option<&SharedInputQueue> {
        self.queue.as_ref()
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// `setActive`: deactivating releases every held input (and sends a neutral frame
    /// while the queue is still attached). The host starts/stops calling `tick`.
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        if !active {
            self.release_all();
        }
        self.active = active;
    }

    pub fn settings(&self) -> &ControllerSettings {
        &self.settings
    }

    /// Replaces every setting at once (for applying stored preferences); the same
    /// clamps as the individual setters apply.
    pub fn set_settings(&mut self, settings: ControllerSettings) {
        self.settings = settings;
        self.set_deadzone(f64::from(settings.deadzone));
        self.set_trigger_sensitivity(f64::from(settings.trigger_sensitivity));
    }

    pub fn deadzone(&self) -> f64 {
        f64::from(self.settings.deadzone)
    }

    /// Clamped to `0.0..=0.9`.
    pub fn set_deadzone(&mut self, value: f64) {
        self.settings.deadzone = value.clamp(0.0, 0.9) as f32;
    }

    pub fn invert_y(&self) -> bool {
        self.settings.invert_y
    }

    pub fn set_invert_y(&mut self, value: bool) {
        self.settings.invert_y = value;
    }

    pub fn swap_ab(&self) -> bool {
        self.settings.swap_ab
    }

    pub fn set_swap_ab(&mut self, value: bool) {
        self.settings.swap_ab = value;
    }

    pub fn trigger_sensitivity(&self) -> f64 {
        f64::from(self.settings.trigger_sensitivity)
    }

    /// Clamped to `0.01..=1.0`.
    pub fn set_trigger_sensitivity(&mut self, value: f64) {
        self.settings.trigger_sensitivity = value.clamp(0.01, 1.0) as f32;
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn controller_name(&self) -> &str {
        &self.controller_name
    }

    pub fn input_focused(&self) -> bool {
        self.focused
    }

    /// `setInputFocused`: losing focus releases everything and sends a neutral frame.
    pub fn set_input_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        info!(target: "Controller", "Input focus: {focused}");
        if !focused {
            self.release_all();
        }
    }

    /// Chord shortcuts recognised on every tick (none by default).
    pub fn set_chord_definitions(&mut self, definitions: Vec<ChordDefinition>) {
        self.chords.set_definitions(definitions);
    }

    pub fn chord_definitions(&self) -> &[ChordDefinition] {
        self.chords.definitions()
    }

    /// Keyboard mapping entry point. Recognised names: a, b, x, y, menu, view, nexus,
    /// dpad-up/down/left/right, left-shoulder, right-shoulder, left-thumb, right-thumb,
    /// left-trigger, right-trigger. Unknown names and input while unfocused are ignored.
    pub fn set_key(&mut self, name: &str, pressed: bool) {
        if !self.focused {
            return;
        }
        if name == "left-trigger" || name == "right-trigger" {
            let side = if name == "left-trigger" {
                TriggerSide::Left
            } else {
                TriggerSide::Right
            };
            self.set_trigger(side, if pressed { 1.0 } else { 0.0 });
            return;
        }
        let Some(button) = button_from_name(name) else {
            return;
        };
        if pressed {
            self.state.buttons.insert(button);
            if let Some(canonical) = canonical_button_name(name) {
                self.pressed.insert(canonical);
            }
        } else {
            self.state.buttons.remove(button);
            self.pressed.remove(name);
        }
    }

    /// Names of the keyboard buttons currently held (the C++ `m_pressed` set).
    pub fn pressed_keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.pressed.iter().copied()
    }

    /// Normalised pointer/keyboard left stick in `-1..=1` (GameController y-up).
    pub fn set_left_stick(&mut self, x: f64, y: f64) {
        if !self.focused {
            return;
        }
        self.state.left_thumb = Vec2::new(x.clamp(-1.0, 1.0) as f32, y.clamp(-1.0, 1.0) as f32);
    }

    /// Normalised pointer/keyboard right stick in `-1..=1` (GameController y-up).
    pub fn set_right_stick(&mut self, x: f64, y: f64) {
        if !self.focused {
            return;
        }
        self.state.right_thumb = Vec2::new(x.clamp(-1.0, 1.0) as f32, y.clamp(-1.0, 1.0) as f32);
    }

    /// Keyboard/pointer trigger value in `0..=1`; above 0.5 counts as pressed.
    pub fn set_trigger(&mut self, side: TriggerSide, value: f64) {
        if !self.focused {
            return;
        }
        let clamped = value.clamp(0.0, 1.0) as f32;
        match side {
            TriggerSide::Left => {
                self.state.left_trigger = clamped;
                self.state.left_trigger_pressed = clamped > 0.5;
            }
            TriggerSide::Right => {
                self.state.right_trigger = clamped;
                self.state.right_trigger_pressed = clamped > 0.5;
            }
        }
    }

    /// `setTrigger(const QString &side, double)`: the C++ string-side variant.
    pub fn set_trigger_by_name(&mut self, side: &str, value: f64) {
        if let Some(side) = TriggerSide::from_name(side) {
            self.set_trigger(side, value);
        }
    }

    /// Clears every held input (for example, when the stream loses focus). While active
    /// and attached to a queue, a neutral frame is enqueued so the server releases too.
    pub fn release_all(&mut self) {
        self.pressed.clear();
        self.state = RawGamepadState::default();
        if self.active
            && let Some(queue) = &self.queue
        {
            queue
                .lock()
                .enqueue_gamepad_frame(self.handler.idle_frame());
        }
    }

    /// One poll (`GamepadInputSource::tick`), to be called every [`POLL_INTERVAL_MS`]
    /// while active. `now_ms` is monotonic time, used for chord hold timing.
    ///
    /// Samples the controller, merges the keyboard state, applies the settings and
    /// enqueues the frame. Input uses unordered, unreliable SCTP, so the current state
    /// is enqueued on every tick (including neutral) and a lost press/release packet
    /// recovers on the next sample.
    ///
    /// While inactive this is a no-op: the Qt class only ran `tick` from the timer that
    /// `updatePolling()` creates on activation and destroys on deactivation, so the
    /// neutral frame `set_active(false)` enqueues is the last one a session sees even
    /// if the host keeps calling `tick`.
    pub fn tick(&mut self, now_ms: u64) -> TickReport {
        let mut report = TickReport::default();
        if !self.active {
            return report;
        }
        let Some(queue) = self.queue.clone() else {
            return report;
        };
        let physical = (self.sampler)();
        self.samples += 1;
        if !physical.state.buttons.is_empty() {
            self.pressed_samples += 1;
        }
        if self.samples.is_multiple_of(SAMPLE_LOG_INTERVAL) {
            info!(
                target: "Controller",
                "samples={} pressedSamples={} focused={}",
                self.samples, self.pressed_samples, self.focused
            );
        }
        if self.connected != physical.connected || self.controller_name != physical.name {
            self.connected = physical.connected;
            self.controller_name = physical.name;
            if self.connected {
                info!(target: "Controller", "Connected: {}", self.controller_name);
            } else {
                info!(target: "Controller", "Disconnected; keyboard input available");
            }
            report.connection_changed = true;
        }

        let mut state = RawGamepadState::default();
        if self.focused {
            if physical.connected {
                state = physical.state;
            }
            state.buttons |= self.state.buttons;
            if self.state.left_thumb != Vec2::ZERO {
                state.left_thumb = self.state.left_thumb;
            }
            if self.state.right_thumb != Vec2::ZERO {
                state.right_thumb = self.state.right_thumb;
            }
            state.left_trigger = state.left_trigger.max(self.state.left_trigger);
            state.right_trigger = state.right_trigger.max(self.state.right_trigger);
            state.left_trigger_pressed |= self.state.left_trigger_pressed;
            state.right_trigger_pressed |= self.state.right_trigger_pressed;
        }
        let frame = self.handler.read_frame(&state, &self.settings);
        report.shortcuts = self.chords.process(&frame, now_ms);
        queue.lock().enqueue_gamepad_frame(frame);
        report
    }
}

/// The `'static` spelling of a recognised button name, so the held-key set needs no
/// allocation.
fn canonical_button_name(name: &str) -> Option<&'static str> {
    const NAMES: [&str; 15] = [
        "a",
        "b",
        "x",
        "y",
        "menu",
        "view",
        "nexus",
        "dpad-up",
        "dpad-down",
        "dpad-left",
        "dpad-right",
        "left-shoulder",
        "right-shoulder",
        "left-thumb",
        "right-thumb",
    ];
    NAMES.iter().copied().find(|candidate| *candidate == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use xcloud_proto::input::models::GamepadInputFrame;
    use xcloud_proto::input::packet::{HEADER_SIZE, gamepad_packet};

    /// `TestGamepadInputSource::payload`: the gamepad block of a one-frame packet.
    fn payload(frame: &GamepadInputFrame) -> Vec<u8> {
        gamepad_packet(0, &[*frame], &[], 0)[HEADER_SIZE..].to_vec()
    }

    fn body(packet: &[u8]) -> &[u8] {
        &packet[HEADER_SIZE..]
    }

    /// A scripted sampler: the test mutates `sample`, the source reads it, `polls` counts.
    struct Scripted {
        sample: Arc<Mutex<NativeGamepadSample>>,
        polls: Arc<AtomicUsize>,
    }

    impl Scripted {
        fn new(sample: NativeGamepadSample) -> Self {
            Self {
                sample: Arc::new(Mutex::new(sample)),
                polls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn sampler(&self) -> Sampler {
            let sample = Arc::clone(&self.sample);
            let polls = Arc::clone(&self.polls);
            Box::new(move || {
                polls.fetch_add(1, Ordering::SeqCst);
                sample.lock().clone()
            })
        }

        fn set(&self, sample: NativeGamepadSample) {
            *self.sample.lock() = sample;
        }

        fn polls(&self) -> usize {
            self.polls.load(Ordering::SeqCst)
        }
    }

    fn flush(queue: &SharedInputQueue) -> Option<Vec<u8>> {
        queue.lock().flush(0)
    }

    // --- tst_gamepadinputsource.cpp -------------------------------------------

    #[test]
    fn physical_input_and_disconnect() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            name: "Test controller".to_string(),
            state: RawGamepadState {
                buttons: GamepadButtons::A | GamepadButtons::MENU | GamepadButtons::RIGHT_THUMB,
                left_thumb: Vec2::new(0.8, -0.7),
                right_thumb: Vec2::new(-0.6, 0.9),
                left_trigger: 0.4,
                right_trigger: 0.7,
                ..RawGamepadState::default()
            },
        };
        let scripted = Scripted::new(sample.clone());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_active(true);
        let report = source.tick(0);
        assert!(scripted.polls() > 0);
        assert!(report.connection_changed);
        assert_eq!(source.controller_name(), sample.name);
        assert!(source.connected());
        let packet = flush(&queue).expect("packet");
        let mut handler = GamepadHandler::default();
        assert_eq!(
            body(&packet),
            payload(&handler.read_frame(&sample.state, &ControllerSettings::default()))
        );

        scripted.set(NativeGamepadSample::default());
        let report = source.tick(POLL_INTERVAL_MS);
        assert!(report.connection_changed);
        assert!(!source.connected());
        assert_eq!(source.controller_name(), "");
        let packet = flush(&queue).expect("packet");
        assert_eq!(body(&packet), payload(&handler.idle_frame()));
    }

    #[test]
    fn focus_loss_and_reconnect_send_neutral_and_fresh_state() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            state: RawGamepadState {
                buttons: GamepadButtons::B,
                ..RawGamepadState::default()
            },
            ..NativeGamepadSample::default()
        };
        let scripted = Scripted::new(sample);
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_active(true);
        let mut now = 0;
        source.tick(now);
        assert!(scripted.polls() > 0);
        flush(&queue);

        source.set_input_focused(false);
        let packet = flush(&queue).expect("neutral frame on focus loss");
        assert_eq!(body(&packet), payload(&GamepadInputFrame::default()));

        let before = scripted.polls();
        now += POLL_INTERVAL_MS;
        source.tick(now);
        assert!(scripted.polls() > before);
        if let Some(packet) = flush(&queue) {
            assert_eq!(body(&packet), payload(&GamepadInputFrame::default()));
        }

        source.set_input_focused(true);
        let resumed = scripted.polls();
        now += POLL_INTERVAL_MS;
        source.tick(now);
        assert!(scripted.polls() > resumed);
        let packet = flush(&queue).expect("fresh state after refocus");
        let expected = GamepadInputFrame {
            buttons: GamepadButtons::B,
            ..GamepadInputFrame::default()
        };
        assert_eq!(body(&packet), payload(&expected));

        source.set_active(false);
        let packet = flush(&queue).expect("neutral frame on deactivate");
        assert_eq!(body(&packet), payload(&GamepadInputFrame::default()));

        let next_session: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        source.set_input_queue(Some(Arc::clone(&next_session)));
        source.set_active(true);
        let restart = scripted.polls();
        now += POLL_INTERVAL_MS;
        source.tick(now);
        assert!(scripted.polls() > restart);
        let packet = flush(&next_session).expect("frame in next session");
        assert_eq!(body(&packet), payload(&expected));
        assert!(flush(&queue).is_none(), "old queue must not receive frames");
    }

    #[test]
    fn repeats_unchanged_input_on_unreliable_channel() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            state: RawGamepadState {
                buttons: GamepadButtons::A,
                ..RawGamepadState::default()
            },
            ..NativeGamepadSample::default()
        };
        let scripted = Scripted::new(sample.clone());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_active(true);
        source.tick(0);
        assert!(scripted.polls() > 0);
        let first = flush(&queue).expect("first packet");

        let before = scripted.polls();
        source.tick(POLL_INTERVAL_MS);
        assert!(scripted.polls() > before);
        let repeat = flush(&queue).expect("repeat packet");
        assert_eq!(body(&first), body(&repeat));

        scripted.set(NativeGamepadSample {
            state: RawGamepadState::default(),
            ..sample
        });
        let before = scripted.polls();
        source.tick(2 * POLL_INTERVAL_MS);
        assert!(scripted.polls() > before);
        flush(&queue); // Simulate losing the release packet.

        let before = scripted.polls();
        source.tick(3 * POLL_INTERVAL_MS);
        assert!(scripted.polls() > before);
        let repeat = flush(&queue).expect("neutral repeat");
        assert_eq!(body(&repeat), payload(&GamepadInputFrame::default()));
    }

    #[test]
    fn keyboard_triggers_and_physical_buttons_combine() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            state: RawGamepadState {
                buttons: GamepadButtons::A,
                ..RawGamepadState::default()
            },
            ..NativeGamepadSample::default()
        };
        let scripted = Scripted::new(sample);
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_key("left-trigger", true);
        source.set_key("x", true);
        source.set_active(true);
        source.tick(0);
        assert!(source.connected());
        let packet = flush(&queue).expect("packet");
        let expected = GamepadInputFrame {
            buttons: GamepadButtons::A | GamepadButtons::X,
            triggers: Vec2::new(1.0, 0.0),
            ..GamepadInputFrame::default()
        };
        assert_eq!(body(&packet), payload(&expected));
    }

    // --- additional coverage ----------------------------------------------------

    #[test]
    fn poll_interval_matches_qt_timer() {
        assert_eq!(POLL_INTERVAL_MS, 8);
    }

    #[test]
    fn tick_without_queue_does_not_sample() {
        let scripted = Scripted::new(NativeGamepadSample::default());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_active(true);
        assert_eq!(source.tick(0), TickReport::default());
        assert_eq!(scripted.polls(), 0);
    }

    /// `updatePolling()`: the Qt timer only exists while active, so `tick` must neither
    /// sample nor enqueue before `setActive(true)` or after `setActive(false)`; the idle
    /// frame from `releaseAll` is the last frame of a session.
    #[test]
    fn tick_while_inactive_does_not_sample_or_enqueue() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            name: "Test controller".to_string(),
            state: RawGamepadState {
                buttons: GamepadButtons::A,
                ..RawGamepadState::default()
            },
        };
        let scripted = Scripted::new(sample);
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));

        // Queue attached but not yet active: nothing is sampled or enqueued.
        assert_eq!(source.tick(0), TickReport::default());
        assert_eq!(scripted.polls(), 0);
        assert!(!source.connected());
        assert!(flush(&queue).is_none(), "no frame before activation");

        source.set_active(true);
        source.tick(POLL_INTERVAL_MS);
        assert_eq!(scripted.polls(), 1);
        assert!(source.connected());
        let live = flush(&queue).expect("live frame while active");
        let expected = GamepadInputFrame {
            buttons: GamepadButtons::A,
            ..GamepadInputFrame::default()
        };
        assert_eq!(body(&live), payload(&expected));

        // Deactivating enqueues the trailing idle frame; later ticks add nothing.
        source.set_active(false);
        assert!(!source.active());
        let idle = flush(&queue).expect("neutral frame on deactivate");
        assert_eq!(body(&idle), payload(&GamepadInputFrame::default()));
        for i in 2..5 {
            assert_eq!(source.tick(i * POLL_INTERVAL_MS), TickReport::default());
        }
        assert_eq!(scripted.polls(), 1, "sampler must not run while inactive");
        assert!(
            flush(&queue).is_none(),
            "idle frame must be the last frame after deactivation"
        );
        assert!(source.connected(), "connection state is left untouched");
    }

    #[test]
    fn button_map_covers_every_setkey_name() {
        let names = [
            ("a", GamepadButtons::A),
            ("b", GamepadButtons::B),
            ("x", GamepadButtons::X),
            ("y", GamepadButtons::Y),
            ("menu", GamepadButtons::MENU),
            ("view", GamepadButtons::VIEW),
            ("nexus", GamepadButtons::NEXUS),
            ("dpad-up", GamepadButtons::DPAD_UP),
            ("dpad-down", GamepadButtons::DPAD_DOWN),
            ("dpad-left", GamepadButtons::DPAD_LEFT),
            ("dpad-right", GamepadButtons::DPAD_RIGHT),
            ("left-shoulder", GamepadButtons::LEFT_SHOULDER),
            ("right-shoulder", GamepadButtons::RIGHT_SHOULDER),
            ("left-thumb", GamepadButtons::LEFT_THUMB),
            ("right-thumb", GamepadButtons::RIGHT_THUMB),
        ];
        for (name, button) in names {
            assert_eq!(button_from_name(name), Some(button), "{name}");
            assert_eq!(canonical_button_name(name), Some(name));
        }
        assert_eq!(button_from_name("left-trigger"), None);
        assert_eq!(button_from_name("bogus"), None);
        assert_eq!(canonical_button_name("bogus"), None);
    }

    #[test]
    fn set_key_press_and_release_track_held_set() {
        let scripted = Scripted::new(NativeGamepadSample::default());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_key("a", true);
        source.set_key("dpad-up", true);
        source.set_key("bogus", true);
        assert_eq!(
            source.state.buttons,
            GamepadButtons::A | GamepadButtons::DPAD_UP
        );
        let mut held: Vec<&str> = source.pressed_keys().collect();
        held.sort_unstable();
        assert_eq!(held, ["a", "dpad-up"]);
        source.set_key("a", false);
        assert_eq!(source.state.buttons, GamepadButtons::DPAD_UP);
        assert_eq!(source.pressed_keys().collect::<Vec<_>>(), ["dpad-up"]);
        source.release_all();
        assert_eq!(source.state, RawGamepadState::default());
        assert_eq!(source.pressed_keys().count(), 0);
    }

    #[test]
    fn unfocused_input_is_ignored() {
        let scripted = Scripted::new(NativeGamepadSample::default());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_focused(false);
        source.set_key("a", true);
        source.set_left_stick(1.0, 1.0);
        source.set_right_stick(-1.0, 0.5);
        source.set_trigger(TriggerSide::Left, 1.0);
        source.set_trigger_by_name("right", 1.0);
        assert_eq!(source.state, RawGamepadState::default());
        assert!(!source.input_focused());
    }

    #[test]
    fn sticks_and_triggers_are_clamped() {
        let scripted = Scripted::new(NativeGamepadSample::default());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_left_stick(2.0, -3.0);
        source.set_right_stick(-7.0, 0.25);
        source.set_trigger(TriggerSide::Left, 4.0);
        source.set_trigger_by_name("right", 0.3);
        source.set_trigger_by_name("sideways", 1.0);
        assert_eq!(source.state.left_thumb, Vec2::new(1.0, -1.0));
        assert_eq!(source.state.right_thumb, Vec2::new(-1.0, 0.25));
        assert_eq!(source.state.left_trigger, 1.0);
        assert!(source.state.left_trigger_pressed);
        assert_eq!(source.state.right_trigger, 0.3);
        assert!(!source.state.right_trigger_pressed);
    }

    #[test]
    fn keyboard_sticks_override_physical_only_when_deflected() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            state: RawGamepadState {
                left_thumb: Vec2::new(0.5, 0.5),
                right_thumb: Vec2::new(-0.5, -0.5),
                ..RawGamepadState::default()
            },
            ..NativeGamepadSample::default()
        };
        let scripted = Scripted::new(sample.clone());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_active(true);
        source.set_left_stick(0.0, 1.0);
        source.tick(0);
        let packet = flush(&queue).expect("packet");
        let mut handler = GamepadHandler::default();
        let expected_state = RawGamepadState {
            left_thumb: Vec2::new(0.0, 1.0),
            ..sample.state
        };
        assert_eq!(
            body(&packet),
            payload(&handler.read_frame(&expected_state, &ControllerSettings::default()))
        );
    }

    #[test]
    fn settings_setters_clamp_like_qbound() {
        let scripted = Scripted::new(NativeGamepadSample::default());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_deadzone(5.0);
        assert!((source.deadzone() - 0.9).abs() < 1e-6);
        source.set_deadzone(-1.0);
        assert_eq!(source.deadzone(), 0.0);
        source.set_trigger_sensitivity(0.0);
        assert!((source.trigger_sensitivity() - 0.01).abs() < 1e-6);
        source.set_trigger_sensitivity(3.0);
        assert_eq!(source.trigger_sensitivity(), 1.0);
        source.set_invert_y(true);
        source.set_swap_ab(true);
        assert!(source.invert_y());
        assert!(source.swap_ab());
        source.set_settings(ControllerSettings {
            deadzone: 2.0,
            trigger_sensitivity: 0.0,
            ..ControllerSettings::default()
        });
        assert!((source.settings().deadzone - 0.9).abs() < 1e-6);
        assert!((source.settings().trigger_sensitivity - 0.01).abs() < 1e-6);
    }

    #[test]
    fn settings_are_applied_through_the_handler() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            state: RawGamepadState {
                buttons: GamepadButtons::A,
                left_thumb: Vec2::new(0.0, 1.0),
                ..RawGamepadState::default()
            },
            ..NativeGamepadSample::default()
        };
        let scripted = Scripted::new(sample);
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_active(true);
        source.set_swap_ab(true);
        source.set_invert_y(true);
        source.tick(0);
        let packet = flush(&queue).expect("packet");
        let expected = GamepadInputFrame {
            buttons: GamepadButtons::B,
            // GameController up (+1) -> protocol up (-1), inverted back to +1.
            left_thumb: Vec2::new(0.0, 1.0),
            ..GamepadInputFrame::default()
        };
        assert_eq!(body(&packet), payload(&expected));
    }

    #[test]
    fn release_all_only_enqueues_while_active_with_queue() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let scripted = Scripted::new(NativeGamepadSample::default());
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.release_all();
        assert!(flush(&queue).is_none());
        source.set_active(true);
        source.set_active(true); // no-op
        source.release_all();
        assert!(queue.lock().has_pending());
    }

    #[test]
    fn chord_shortcuts_are_reported_from_tick() {
        let queue: SharedInputQueue = Arc::new(Mutex::new(InputQueue::new()));
        let sample = NativeGamepadSample {
            connected: true,
            state: RawGamepadState {
                buttons: GamepadButtons::MENU | GamepadButtons::VIEW,
                ..RawGamepadState::default()
            },
            ..NativeGamepadSample::default()
        };
        let scripted = Scripted::new(sample);
        let mut source = GamepadInputSource::new(scripted.sampler());
        source.set_input_queue(Some(Arc::clone(&queue)));
        source.set_active(true);
        source.set_chord_definitions(vec![ChordDefinition::new(
            GamepadButtons::MENU | GamepadButtons::VIEW,
            500,
            ShortcutAction::ToggleStreamMenu,
        )]);
        assert_eq!(source.chord_definitions().len(), 1);
        assert!(source.tick(0).shortcuts.is_empty());
        assert!(source.tick(100).shortcuts.is_empty());
        assert_eq!(
            source.tick(500).shortcuts,
            vec![ShortcutAction::ToggleStreamMenu]
        );
        assert!(source.tick(600).shortcuts.is_empty(), "fires once per hold");
    }
}
