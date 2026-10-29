//! Port of qt/src/app/SessionController.{h,cpp}: the Microsoft device-code sign-in state
//! machine, on the [`crate::state`] contract. The controller owns the
//! `Arc<RwLock<SessionSnapshot>>` the UI reads, drains `SessionCommand`s from the
//! handle's channel on a tokio task and calls [`Repaint`] after every snapshot change.
//!
//! Signals became values: `tokensChanged` + `authenticated` are
//! [`SessionEvent::Authenticated`] (carrying the new `StreamTokens`), the
//! `tokensChanged` of `signOut` is [`SessionEvent::SignedOut`], and the
//! `updateApiCredentials()` call of `AppController::bootstrap` (the stored bundle the C++
//! constructor had loaded into `m_tokens`) is [`SessionEvent::TokensRestored`]. The app
//! shell drains the event receiver once per frame and hands each event to
//! `Services::handle_session_event` (the port of `AppController::wireSession`).
//!
//! An event is always queued *before* the snapshot change that goes with it and every
//! `emit` requests a repaint, so a frame that sees `state == Authenticated` also finds the
//! `Authenticated` event in the channel: the C++ did `tokensChanged` → `setState` →
//! `authenticated` synchronously in one call and the UI could never observe the state
//! without the credentials having been applied.
//!
//! The stored stream tokens are read once, on the controller task (tokio), when it starts:
//! the C++ `SettingsTokenStore` was a cheap `QSettings` read in the constructor, the
//! keychain-backed store is a blocking call that may raise an access prompt, so it never
//! runs on the UI thread that calls [`spawn`] or `Services::bootstrap`.
//!
//! Status texts, `lastError`, `isBusy` and the state transitions follow
//! `SessionController.cpp` line by line:
//!
//! - `restoreSession`: renew saved stream tokens before authenticating; no stored refresh
//!   token → `Unauthenticated`; otherwise busy + "Signing in" + `Unknown` while the stream
//!   tokens are refreshed, then `Authenticated` or (session cleared) `Unauthenticated`.
//! - `beginSignIn`: ignored while busy; busy + "Contacting Microsoft" → device code →
//!   `Authenticating` + "Waiting for you to finish signing in" → poll (device-code
//!   interval, 900 s cap) → "Loading your Game Pass library" → exchange → `Authenticated`.
//!   Any failure: not busy, empty status, `lastError`, `Error`.
//! - `cancelSignIn`: not busy, empty status, device code cleared, `Unauthenticated`. The
//!   in-flight request and poll are abandoned (the C++ `m_cancelled` flag); like the C++,
//!   an exchange that has already started still completes and signs the user in.
//! - `signOut`: session cleared, tokens reset, error and status emptied, not busy,
//!   `Unauthenticated`. Unlike the C++ this also abandons an in-flight exchange, so a
//!   sign-out can never be followed by a surprise `Authenticated`.
//!
//! The command receiver is a crossbeam channel (the UI side is synchronous), so a tiny
//! forwarding thread bridges it into a tokio channel the task can `select!` on together
//! with the in-flight sign-in future. `RestoreSession` while another operation is in
//! flight is ignored with a warning (the C++ would have run both at once).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use xcloud_api::auth::{
    DEFAULT_POLL_MAX_WAIT_SECS, DeviceCodeInfo, MicrosoftAuthService, StreamTokens,
};

use crate::services::Services;
use crate::state::{AuthState, Handle, Repaint, SessionCommand, SessionHandle, SessionSnapshot};

const LOG: &str = "Session";

/// What `SessionController` signalled to `AppController`.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// The `updateApiCredentials()` of `AppController::bootstrap`: the stored bundle
    /// (`m_tokens` as the C++ constructor loaded it, possibly empty or stale) to hand to
    /// the API client before the restore proceeds.
    TokensRestored(Box<StreamTokens>),
    /// `tokensChanged` followed by `authenticated`: the stream tokens to hand to the API
    /// client; the library should refresh.
    Authenticated(Box<StreamTokens>),
    /// `tokensChanged` after `signOut`: the tokens are empty now.
    SignedOut,
}

/// Where the sign-in job is, so `cancelSignIn` can tell whether abandoning it still
/// matters (the C++ exchange callback did not check `m_cancelled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Phase {
    Idle = 0,
    DeviceCode = 1,
    Polling = 2,
    Exchanging = 3,
}

impl Phase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Phase::DeviceCode,
            2 => Phase::Polling,
            3 => Phase::Exchanging,
            _ => Phase::Idle,
        }
    }
}

/// The controller state shared between the command loop and the in-flight job.
pub struct SessionController {
    auth: Arc<MicrosoftAuthService>,
    snapshot: Arc<RwLock<SessionSnapshot>>,
    repaint: Repaint,
    events: Sender<SessionEvent>,
    /// `m_tokens`; loaded from the store by [`Self::run`] on the controller task.
    tokens: Mutex<StreamTokens>,
    phase: AtomicU8,
}

impl std::fmt::Debug for SessionController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionController")
            .field("snapshot", &*self.snapshot.read())
            .field("phase", &Phase::from_u8(self.phase.load(Ordering::SeqCst)))
            .finish_non_exhaustive()
    }
}

/// Builds the controller on `services.runtime`. Returns the UI handle and the receiver of
/// the events `AppController::wireSession` reacted to.
pub fn spawn(services: &Services, repaint: Repaint) -> (SessionHandle, Receiver<SessionEvent>) {
    let (handle, snapshot, commands) = Handle::new(SessionSnapshot::default());
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let controller = Arc::new(SessionController::new(
        Arc::clone(&services.auth),
        snapshot,
        repaint,
        events_tx,
    ));
    let inbox = forward_commands(commands);
    services.runtime.spawn(controller.run(inbox));
    (handle, events_rx)
}

/// Bridges the synchronous command channel into one the tokio task can await. The thread
/// ends when every `SessionHandle` is dropped, which also ends the task.
fn forward_commands(commands: Receiver<SessionCommand>) -> mpsc::UnboundedReceiver<SessionCommand> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("session-commands".to_owned())
        .spawn(move || {
            while let Ok(command) = commands.recv() {
                if tx.send(command).is_err() {
                    break;
                }
            }
        });
    if let Err(error) = spawned {
        tracing::error!(target: LOG, "cannot start the command forwarding thread: {error}");
    }
    rx
}

/// The one operation that may be in flight (`restoreSession`'s refresh or `beginSignIn`).
struct Job {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    active: bool,
}

impl Job {
    fn idle() -> Self {
        Self {
            future: Box::pin(std::future::pending()),
            active: false,
        }
    }

    fn start(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.future = Box::pin(future);
        self.active = true;
    }

    fn abort(&mut self) {
        *self = Self::idle();
    }
}

impl SessionController {
    fn new(
        auth: Arc<MicrosoftAuthService>,
        snapshot: Arc<RwLock<SessionSnapshot>>,
        repaint: Repaint,
        events: Sender<SessionEvent>,
    ) -> Self {
        Self {
            auth,
            snapshot,
            repaint,
            events,
            tokens: Mutex::new(StreamTokens::default()),
            phase: AtomicU8::new(Phase::Idle as u8),
        }
    }

    /// The command loop: runs until every `SessionHandle` is gone.
    async fn run(self: Arc<Self>, mut commands: mpsc::UnboundedReceiver<SessionCommand>) {
        // The C++ constructor loaded the persisted bundle into `m_tokens`; here it is the
        // first thing the task does, so the (possibly blocking) store read never happens
        // on the thread that built the controller, and happens exactly once.
        self.load_stored_tokens();
        let mut job = Job::idle();
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    self.handle_command(command, &mut job);
                }
                () = &mut job.future => {
                    job.abort();
                }
            }
        }
        job.abort();
        tracing::debug!(target: LOG, "session controller stopped");
    }

    fn handle_command(self: &Arc<Self>, command: SessionCommand, job: &mut Job) {
        match command {
            SessionCommand::RestoreSession => {
                if job.active {
                    tracing::warn!(target: LOG, "restoreSession ignored: an operation is in flight");
                    return;
                }
                job.start(Arc::clone(self).restore_session());
            }
            SessionCommand::BeginSignIn => {
                // `if (!m_auth || m_isBusy) return;`
                if job.active || self.snapshot.read().is_busy {
                    return;
                }
                job.start(Arc::clone(self).begin_sign_in());
            }
            SessionCommand::CancelSignIn => {
                // `m_cancelled = true`: the pending device-code request / poll callbacks
                // become no-ops. The exchange callback never checked the flag.
                if job.active && self.phase() != Phase::Exchanging {
                    job.abort();
                    self.set_phase(Phase::Idle);
                }
                self.set_busy(false);
                self.set_status("");
                self.set_device_code(None);
                self.set_state(AuthState::Unauthenticated);
            }
            SessionCommand::SignOut => {
                job.abort();
                self.set_phase(Phase::Idle);
                if let Err(error) = self.auth.clear_session() {
                    tracing::warn!(target: LOG, "clearing the session failed: {error}");
                }
                *self.tokens.lock() = StreamTokens::default();
                self.emit(SessionEvent::SignedOut);
                self.set_error("");
                self.set_busy(false);
                self.set_status("");
                self.set_state(AuthState::Unauthenticated);
            }
        }
    }

    /// `m_tokens = m_auth->restoreStreamTokens()` of the C++ constructor.
    fn load_stored_tokens(&self) {
        let tokens = self.auth.restore_stream_tokens().unwrap_or_else(|error| {
            tracing::warn!(target: LOG, "cannot read the stored stream tokens: {error}");
            StreamTokens::default()
        });
        *self.tokens.lock() = tokens;
    }

    /// `AppController::bootstrap`: `updateApiCredentials()` with the stored bundle, then
    /// `SessionController::restoreSession`.
    async fn restore_session(self: Arc<Self>) {
        let tokens = self.tokens.lock().clone();
        self.emit(SessionEvent::TokensRestored(Box::new(tokens.clone())));
        // is_valid only means a token is present; persisted GSSV tokens may be
        // expired. Always renew using the stored refresh token before authenticating.
        let has_session = self.auth.has_stored_session().unwrap_or_else(|error| {
            tracing::warn!(target: LOG, "cannot read the stored refresh token: {error}");
            false
        });
        if !has_session {
            self.set_state(AuthState::Unauthenticated);
            return;
        }

        self.set_busy(true);
        self.set_status("Signing in");
        self.set_state(AuthState::Unknown);
        let result = self.auth.refresh_stream_tokens().await;
        self.set_busy(false);
        self.set_status("");
        match result {
            Ok(tokens) => {
                self.set_tokens(tokens.clone());
                self.emit(SessionEvent::Authenticated(Box::new(tokens)));
                self.set_state(AuthState::Authenticated);
            }
            Err(error) => {
                tracing::warn!(target: LOG, "Session restore failed: {error}");
                if let Err(clear_error) = self.auth.clear_session() {
                    tracing::warn!(target: LOG, "clearing the stale session failed: {clear_error}");
                }
                self.set_state(AuthState::Unauthenticated);
            }
        }
    }

    /// `SessionController::beginSignIn`: device code → poll → stream token exchange.
    async fn begin_sign_in(self: Arc<Self>) {
        self.set_phase(Phase::DeviceCode);
        self.set_error("");
        self.set_busy(true);
        self.set_status("Contacting Microsoft");

        let info = match self.auth.request_device_code().await {
            Ok(info) => info,
            Err(error) => {
                self.fail(&error.to_string());
                return;
            }
        };
        self.set_device_code(Some(&info));
        self.set_state(AuthState::Authenticating);
        self.set_status("Waiting for you to finish signing in");

        self.set_phase(Phase::Polling);
        // `pollForMsaToken(deviceCode, interval)` with the default 900 s cap; the service
        // clamps the interval to at least one second like the C++ `qMax(1, ...)`.
        let interval = u64::try_from(info.interval).unwrap_or(1);
        let msa_token = match self
            .auth
            .poll_for_msa_token(&info.device_code, interval, DEFAULT_POLL_MAX_WAIT_SECS)
            .await
        {
            Ok(token) => token,
            Err(error) => {
                self.fail(&error.to_string());
                return;
            }
        };

        self.set_status("Loading your Game Pass library");
        self.set_phase(Phase::Exchanging);
        let result = self.auth.exchange_for_stream_tokens(&msa_token).await;
        self.set_phase(Phase::Idle);
        self.set_busy(false);
        self.set_status("");
        match result {
            Ok(tokens) => {
                self.set_tokens(tokens.clone());
                self.emit(SessionEvent::Authenticated(Box::new(tokens)));
                self.set_state(AuthState::Authenticated);
            }
            Err(error) => {
                self.set_error(&error.to_string());
                self.set_state(AuthState::Error);
            }
        }
    }

    /// The failure epilogue of the device-code and poll steps.
    fn fail(&self, error: &str) {
        self.set_phase(Phase::Idle);
        self.set_busy(false);
        self.set_status("");
        self.set_error(error);
        self.set_state(AuthState::Error);
    }

    fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::SeqCst);
    }

    /// `m_tokens = ...; emit tokensChanged();` (the event itself is sent with the
    /// `authenticated` notification by the caller).
    fn set_tokens(&self, tokens: StreamTokens) {
        *self.tokens.lock() = tokens;
    }

    /// Queues an event for the app shell and asks for a frame so it is drained promptly
    /// (the C++ signal handlers ran synchronously; here the shell drains once per frame).
    fn emit(&self, event: SessionEvent) {
        if self.events.send(event).is_err() {
            tracing::debug!(target: LOG, "no session event listener");
        }
        (self.repaint)();
    }

    /// Applies `change` to the snapshot and repaints when it reports a difference.
    fn update(&self, change: impl FnOnce(&mut SessionSnapshot) -> bool) {
        let changed = change(&mut self.snapshot.write());
        if changed {
            (self.repaint)();
        }
    }

    fn set_state(&self, state: AuthState) {
        self.update(|snapshot| {
            if snapshot.state == state {
                return false;
            }
            snapshot.state = state;
            true
        });
    }

    fn set_busy(&self, busy: bool) {
        self.update(|snapshot| {
            if snapshot.is_busy == busy {
                return false;
            }
            snapshot.is_busy = busy;
            true
        });
    }

    fn set_error(&self, error: &str) {
        self.update(|snapshot| {
            if snapshot.last_error == error {
                return false;
            }
            snapshot.last_error = error.to_owned();
            true
        });
    }

    fn set_status(&self, status: &str) {
        self.update(|snapshot| {
            if snapshot.status_text == status {
                return false;
            }
            snapshot.status_text = status.to_owned();
            true
        });
    }

    /// `m_deviceCode = ...; m_signInUrl = ...; emit deviceCodeChanged();` `None` clears
    /// the fields as `cancelSignIn` does.
    fn set_device_code(&self, info: Option<&DeviceCodeInfo>) {
        let (user_code, uri, complete, sign_in_url) = match info {
            Some(info) => (
                info.user_code.clone(),
                info.verification_uri.clone(),
                info.verification_uri_complete.clone(),
                info.sign_in_url(),
            ),
            None => Default::default(),
        };
        self.update(|snapshot| {
            if snapshot.user_code == user_code
                && snapshot.verification_uri == uri
                && snapshot.verification_uri_complete == complete
                && snapshot.sign_in_url == sign_in_url
            {
                return false;
            }
            snapshot.user_code = user_code;
            snapshot.verification_uri = uri;
            snapshot.verification_uri_complete = complete;
            snapshot.sign_in_url = sign_in_url;
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::runtime::Handle as RuntimeHandle;
    use xcloud_api::token_store::{MemoryTokenStore, TokenStore};
    use xcloud_api::transport::FakeTransport;
    use xcloud_webrtc::null::NullTransport;

    use super::*;

    const DEVICE_CODE_JSON: &str = r#"{"device_code":"DEV","user_code":"ABCD-EFGH","verification_uri":"https://microsoft.com/devicelogin","verification_uri_complete":"https://microsoft.com/devicelogin?otc=ABCD-EFGH","expires_in":900,"interval":5}"#;
    const PENDING_JSON: &str = r#"{"error":"authorization_pending"}"#;
    const MSA_TOKEN_JSON: &str = r#"{"access_token":"MSATOKEN","refresh_token":"REFRESH"}"#;
    const USER_TOKEN_JSON: &str = r#"{"Token":"USER","DisplayClaims":{"xui":[{"uhs":"U"}]}}"#;

    /// What the repaint callback can see: the UI handle and the event channel, exactly
    /// what the app shell reads when a frame runs.
    type Probe = Arc<Mutex<Option<(SessionHandle, Receiver<SessionEvent>)>>>;

    struct Harness {
        transport: Arc<FakeTransport>,
        store: Arc<MemoryTokenStore>,
        session: SessionHandle,
        events: Receiver<SessionEvent>,
        repaints: Arc<AtomicUsize>,
        /// One entry per repaint that first showed `Authenticated`: how many events were
        /// queued at that moment. A frame drawn then drains them all, so the count must
        /// equal the events the test receives afterwards (the `Authenticated` one
        /// included), or the frame would show the library before the credentials and the
        /// refresh had been applied.
        events_queued_at_first_authenticated_frame: Arc<Mutex<Vec<usize>>>,
    }

    fn harness_with_store(store: Arc<MemoryTokenStore>) -> Harness {
        let transport = Arc::new(FakeTransport::new());
        let services = Services::with_parts(
            transport.clone(),
            store.clone(),
            Arc::new(NullTransport::new()),
            RuntimeHandle::current(),
        );
        let repaints = Arc::new(AtomicUsize::new(0));
        let events_queued_at_first_authenticated_frame = Arc::new(Mutex::new(Vec::new()));
        let probe: Probe = Arc::new(Mutex::new(None));
        let repaint: Repaint = {
            let counter = repaints.clone();
            let log = events_queued_at_first_authenticated_frame.clone();
            let probe = probe.clone();
            let last_state = Arc::new(Mutex::new(AuthState::Unknown));
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                if let Some((session, events)) = &*probe.lock() {
                    let state = session.read().state;
                    let mut last = last_state.lock();
                    if state == AuthState::Authenticated && *last != AuthState::Authenticated {
                        // `len()` only inspects; `iter()`/`try_recv()` would consume.
                        log.lock().push(events.len());
                    }
                    *last = state;
                }
            })
        };
        let (session, events) = spawn(&services, repaint);
        // The task has not run yet (current-thread runtime), so this is race-free.
        *probe.lock() = Some((session.clone(), events.clone()));
        Harness {
            transport,
            store,
            session,
            events,
            repaints,
            events_queued_at_first_authenticated_frame,
        }
    }

    fn harness() -> Harness {
        harness_with_store(Arc::new(MemoryTokenStore::new()))
    }

    /// Wait for the OS command-forwarding thread, which does not follow tokio's paused
    /// clock. The snapshot may already be unauthenticated before a restore is processed.
    async fn wait_for_tokens_restored(h: &Harness, expected: &StreamTokens) {
        let events = h.events.clone();
        let event =
            tokio::task::spawn_blocking(move || events.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap();
        assert_eq!(
            event,
            Ok(SessionEvent::TokensRestored(Box::new(expected.clone())))
        );
    }

    /// `RestoreSession` always reports the stored bundle first (`bootstrap` →
    /// `updateApiCredentials`).
    fn expect_tokens_restored(h: &Harness, expected: &StreamTokens) {
        assert_eq!(
            h.events.try_recv(),
            Ok(SessionEvent::TokensRestored(Box::new(expected.clone())))
        );
    }

    fn stored_tokens() -> StreamTokens {
        StreamTokens {
            xcloud_token: "STORED".into(),
            xcloud_host: "https://stored.example".into(),
            xhome_token: "HOME".into(),
            xhome_host: "https://home.example".into(),
            ..StreamTokens::default()
        }
    }

    /// Queues the Xbox Live + GSSV replies `exchange_for_stream_tokens` consumes.
    fn enqueue_exchange(transport: &FakeTransport) {
        transport.enqueue_json("user.auth.xboxlive.com", USER_TOKEN_JSON, 200);
        transport.enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"WEBXSTS"}"#, 200);
        transport.enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"STREAMXSTS"}"#, 200);
        transport.enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        transport.enqueue_json(
            "xgpuweb.gssv",
            r#"{"gsToken":"XCLOUD","offeringSettings":{"regions":[{"name":"weu","baseUri":"https://weu.gssv-play-prod.xboxlive.com","isDefault":true}]}}"#,
            200,
        );
        transport.enqueue_json("xgpuwebf2p.gssv", r#"{"gsToken":"F2P"}"#, 200);
    }

    /// Steps virtual time in 1 ms slices until `predicate` holds (at most 200 s).
    async fn wait_until(session: &SessionHandle, predicate: impl Fn(&SessionSnapshot) -> bool) {
        for _ in 0..200_000 {
            if session.with(&predicate) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "timed out waiting for the session state: {:?}",
            session.read()
        );
    }

    fn count_requests(transport: &FakeTransport, url_contains: &str) -> usize {
        transport
            .requests()
            .iter()
            .filter(|request| request.url.contains(url_contains))
            .count()
    }

    #[tokio::test(start_paused = true)]
    async fn restore_without_stored_session_is_unauthenticated() {
        let h = harness();
        assert_eq!(h.session.read(), SessionSnapshot::default());

        h.session.send(SessionCommand::RestoreSession);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;

        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert_eq!(snapshot.last_error, "");
        assert_eq!(h.transport.request_count(), 0);
        expect_tokens_restored(&h, &StreamTokens::default());
        assert!(h.events.try_recv().is_err());
        assert!(h.repaints.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stored_stream_tokens_without_refresh_token_require_sign_in() {
        let store = Arc::new(MemoryTokenStore::new());
        store.save_stream_tokens(&stored_tokens()).unwrap();
        let h = harness_with_store(store);
        h.session.send(SessionCommand::RestoreSession);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;
        expect_tokens_restored(&h, &stored_tokens());
        assert!(h.events.try_recv().is_err());
        assert_eq!(h.transport.request_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn restore_with_refresh_token_refreshes_stream_tokens() {
        let store = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("R1").unwrap();
        store.save_stream_tokens(&stored_tokens()).unwrap();
        let h = harness_with_store(store);
        // A delayed reply keeps the busy phase observable in virtual time.
        h.transport.enqueue_json_delayed(
            "oauth2/v2.0/token",
            r#"{"access_token":"MSA","refresh_token":"R2"}"#,
            50,
        );
        enqueue_exchange(&h.transport);

        h.session.send(SessionCommand::RestoreSession);
        wait_until(&h.session, |s| s.is_busy).await;
        let busy = h.session.read();
        assert_eq!(busy.state, AuthState::Unknown);
        assert_eq!(busy.status_text, "Signing in");

        wait_until(&h.session, |s| s.state == AuthState::Authenticated).await;
        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert_eq!(snapshot.last_error, "");
        // The cached token is present, but must not bypass renewal.
        expect_tokens_restored(&h, &stored_tokens());
        match h.events.try_recv() {
            Ok(SessionEvent::Authenticated(tokens)) => {
                assert_eq!(tokens.xcloud_token, "XCLOUD");
                assert_eq!(
                    tokens.xcloud_host,
                    "https://weu.gssv-play-prod.xboxlive.com"
                );
                assert_eq!(tokens.xcloud_f2p_token, "F2P");
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert_eq!(h.store.load_refresh_token().unwrap(), "R2");
        assert_eq!(h.transport.pending_count(), 0);
        // TokensRestored + Authenticated were both queued before the state showed.
        assert_eq!(
            *h.events_queued_at_first_authenticated_frame.lock(),
            vec![2]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn restore_failure_clears_the_session() {
        let store = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("R1").unwrap();
        store.save_msa_token("OLD").unwrap();
        let h = harness_with_store(store);
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"error":"invalid_grant"}"#, 400);

        h.session.send(SessionCommand::RestoreSession);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;

        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        // restoreSession never sets lastError.
        assert_eq!(snapshot.last_error, "");
        assert_eq!(h.store.load_refresh_token().unwrap(), "");
        assert_eq!(h.store.load_msa_token().unwrap(), "");
        expect_tokens_restored(&h, &StreamTokens::default());
        assert!(h.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn full_device_code_flow_signs_in() {
        let h = harness();
        h.transport
            .enqueue_json("devicecode", DEVICE_CODE_JSON, 200);
        h.transport
            .enqueue_json("oauth2/v2.0/token", PENDING_JSON, 400);
        h.transport
            .enqueue_json("oauth2/v2.0/token", MSA_TOKEN_JSON, 200);
        enqueue_exchange(&h.transport);
        let started = tokio::time::Instant::now();

        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Authenticating).await;
        let authenticating = h.session.read();
        assert!(authenticating.is_busy);
        assert_eq!(
            authenticating.status_text,
            "Waiting for you to finish signing in"
        );
        assert_eq!(authenticating.user_code, "ABCD-EFGH");
        assert_eq!(
            authenticating.verification_uri,
            "https://microsoft.com/devicelogin"
        );
        assert_eq!(
            authenticating.verification_uri_complete,
            "https://microsoft.com/devicelogin?otc=ABCD-EFGH"
        );
        assert_eq!(
            authenticating.sign_in_url,
            "https://microsoft.com/devicelogin?otc=ABCD-EFGH"
        );
        assert_eq!(authenticating.last_error, "");

        // A second BeginSignIn while busy is ignored.
        h.session.send(SessionCommand::BeginSignIn);

        wait_until(&h.session, |s| s.state == AuthState::Authenticated).await;
        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert_eq!(snapshot.last_error, "");
        // The device code stays visible until cancel/sign-out, as in the C++.
        assert_eq!(snapshot.user_code, "ABCD-EFGH");

        // Two poll attempts, one interval (5 s) before each.
        assert!(started.elapsed() >= Duration::from_secs(10));
        assert_eq!(count_requests(&h.transport, "devicecode"), 1);
        assert_eq!(count_requests(&h.transport, "oauth2/v2.0/token"), 2);
        assert_eq!(h.transport.pending_count(), 0);

        match h.events.try_recv() {
            Ok(SessionEvent::Authenticated(tokens)) => {
                assert_eq!(tokens.xcloud_token, "XCLOUD");
                assert_eq!(tokens.web_token, "WEBXSTS");
                assert_eq!(h.store.load_stream_tokens().unwrap(), *tokens);
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(h.events.try_recv().is_err());
        assert_eq!(h.store.load_refresh_token().unwrap(), "REFRESH");
        // `tokensChanged` → `setState` → `authenticated` were one synchronous call in the
        // C++; the frame that first shows `authenticated` must already see the event.
        assert_eq!(
            *h.events_queued_at_first_authenticated_frame.lock(),
            vec![1]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_mid_poll_returns_to_unauthenticated() {
        let h = harness();
        h.transport
            .enqueue_json("devicecode", DEVICE_CODE_JSON, 200);
        for _ in 0..8 {
            h.transport
                .enqueue_json("oauth2/v2.0/token", PENDING_JSON, 400);
        }

        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Authenticating).await;
        // Let one poll attempt go out, then cancel while the next interval is pending.
        for _ in 0..200_000 {
            if count_requests(&h.transport, "oauth2/v2.0/token") >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let polls_before_cancel = count_requests(&h.transport, "oauth2/v2.0/token");
        assert!(polls_before_cancel >= 1);

        h.session.send(SessionCommand::CancelSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;
        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert_eq!(snapshot.user_code, "");
        assert_eq!(snapshot.verification_uri, "");
        assert_eq!(snapshot.verification_uri_complete, "");
        assert_eq!(snapshot.sign_in_url, "");
        assert_eq!(snapshot.last_error, "");

        // The poll was abandoned: nothing else goes out, even after many intervals.
        let polls_at_cancel = count_requests(&h.transport, "oauth2/v2.0/token");
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            count_requests(&h.transport, "oauth2/v2.0/token"),
            polls_at_cancel
        );
        assert_eq!(h.session.read().state, AuthState::Unauthenticated);
        assert!(h.events.try_recv().is_err());

        // Sign-in can start again afterwards.
        h.transport.enqueue_error("devicecode", "dns");
        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Error).await;
        assert_eq!(count_requests(&h.transport, "devicecode"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_the_exchange_still_signs_in_like_the_cpp() {
        let h = harness();
        h.transport
            .enqueue_json("devicecode", DEVICE_CODE_JSON, 200);
        h.transport
            .enqueue_json("oauth2/v2.0/token", MSA_TOKEN_JSON, 200);
        h.transport
            .enqueue_json_delayed("user.auth.xboxlive.com", USER_TOKEN_JSON, 100);
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"WEBXSTS"}"#, 200);
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"STREAMXSTS"}"#, 200);
        h.transport
            .enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        h.transport
            .enqueue_json("xgpuweb.gssv", r#"{"gsToken":"XCLOUD"}"#, 200);
        h.transport
            .enqueue_json("xgpuwebf2p.gssv", r#"{"gsToken":"F2P"}"#, 200);

        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| {
            s.status_text == "Loading your Game Pass library"
        })
        .await;

        h.session.send(SessionCommand::CancelSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;
        assert_eq!(h.session.read().user_code, "");

        // The exchange callback ignored `m_cancelled`: the sign-in completes anyway.
        wait_until(&h.session, |s| s.state == AuthState::Authenticated).await;
        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert!(matches!(
            h.events.try_recv(),
            Ok(SessionEvent::Authenticated(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn device_code_request_failure_enters_error_state() {
        let h = harness();
        h.transport.enqueue_error("devicecode", "dns");

        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Error).await;

        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert!(
            snapshot.last_error.contains("dns"),
            "{}",
            snapshot.last_error
        );
        assert_eq!(snapshot.user_code, "");
        assert!(h.events.try_recv().is_err());

        // beginSignIn clears the previous error first.
        h.transport
            .enqueue_json("devicecode", DEVICE_CODE_JSON, 200);
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"error":"authorization_declined"}"#,
            400,
        );
        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Authenticating).await;
        assert_eq!(h.session.read().last_error, "");
        wait_until(&h.session, |s| s.state == AuthState::Error).await;
        let snapshot = h.session.read();
        assert!(
            snapshot.last_error.contains("oauth:authorization_declined"),
            "{}",
            snapshot.last_error
        );
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        // The failed poll leaves the device code in place (only cancel clears it).
        assert_eq!(snapshot.user_code, "ABCD-EFGH");
    }

    #[tokio::test(start_paused = true)]
    async fn exchange_failure_enters_error_state() {
        let h = harness();
        h.transport
            .enqueue_json("devicecode", DEVICE_CODE_JSON, 200);
        h.transport
            .enqueue_json("oauth2/v2.0/token", MSA_TOKEN_JSON, 200);
        h.transport
            .enqueue_json("user.auth.xboxlive.com", r#"{"XErr":2148916233}"#, 401);

        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Error).await;

        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert!(!snapshot.last_error.is_empty());
        assert!(h.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn sign_out_clears_session_and_tokens() {
        let store = Arc::new(MemoryTokenStore::new());
        store.save_stream_tokens(&stored_tokens()).unwrap();
        store.save_refresh_token("R1").unwrap();
        let h = harness_with_store(store);
        h.transport
            .enqueue_json("oauth2/v2.0/token", MSA_TOKEN_JSON, 200);
        enqueue_exchange(&h.transport);

        h.session.send(SessionCommand::RestoreSession);
        wait_until(&h.session, |s| s.state == AuthState::Authenticated).await;
        expect_tokens_restored(&h, &stored_tokens());
        assert!(matches!(
            h.events.try_recv(),
            Ok(SessionEvent::Authenticated(_))
        ));

        h.session.send(SessionCommand::SignOut);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;

        let snapshot = h.session.read();
        assert!(!snapshot.is_busy);
        assert_eq!(snapshot.status_text, "");
        assert_eq!(snapshot.last_error, "");
        assert_eq!(h.events.try_recv(), Ok(SessionEvent::SignedOut));
        assert_eq!(
            h.store.load_stream_tokens().unwrap(),
            StreamTokens::default()
        );
        assert_eq!(h.store.load_refresh_token().unwrap(), "");

        let request_count = h.transport.request_count();
        // Restoring now finds nothing.
        h.session.send(SessionCommand::RestoreSession);
        wait_for_tokens_restored(&h, &StreamTokens::default()).await;
        assert_eq!(h.session.read().state, AuthState::Unauthenticated);
        assert_eq!(h.transport.request_count(), request_count);
        assert!(h.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn stored_tokens_are_read_once_on_the_controller_task() {
        let h = harness();
        // Building the controller and asking for the restore (what the UI thread does in
        // `spawn` + `bootstrap`) must not read the store on the caller's thread: tokens
        // saved after both calls, before the task has run, are what the task loads.
        h.session.send(SessionCommand::RestoreSession);
        h.store.save_stream_tokens(&stored_tokens()).unwrap();

        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;
        expect_tokens_restored(&h, &stored_tokens());
        assert!(h.events.try_recv().is_err());
        assert_eq!(h.transport.request_count(), 0);

        // A later restore reuses `m_tokens` (loaded once, like the C++ constructor's
        // read) instead of hitting the store again.
        let replaced = StreamTokens {
            xcloud_token: "REPLACED".into(),
            xcloud_host: "https://replaced.example".into(),
            ..StreamTokens::default()
        };
        h.store.save_stream_tokens(&replaced).unwrap();
        h.session.send(SessionCommand::RestoreSession);
        wait_for_tokens_restored(&h, &stored_tokens()).await;
        assert!(h.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_stored_tokens_are_reported_before_the_refresh() {
        // Bootstrap reports the cached F2P bundle, but authentication still waits
        // for renewed stream tokens.
        let store = Arc::new(MemoryTokenStore::new());
        let stale = StreamTokens {
            xcloud_f2p_token: "F2P-OLD".into(),
            xcloud_f2p_host: "https://f2p.example".into(),
            ..StreamTokens::default()
        };
        store.save_stream_tokens(&stale).unwrap();
        store.save_refresh_token("R1").unwrap();
        let h = harness_with_store(store);
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"access_token":"MSA","refresh_token":"R2"}"#,
            200,
        );
        enqueue_exchange(&h.transport);

        h.session.send(SessionCommand::RestoreSession);
        wait_until(&h.session, |s| s.state == AuthState::Authenticated).await;

        expect_tokens_restored(&h, &stale);
        assert!(matches!(
            h.events.try_recv(),
            Ok(SessionEvent::Authenticated(tokens)) if tokens.xcloud_token == "XCLOUD"
        ));
        assert!(h.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn sign_out_during_sign_in_abandons_it() {
        let h = harness();
        h.transport
            .enqueue_json("devicecode", DEVICE_CODE_JSON, 200);
        h.transport
            .enqueue_json("oauth2/v2.0/token", PENDING_JSON, 400);
        h.transport
            .enqueue_json("oauth2/v2.0/token", MSA_TOKEN_JSON, 200);
        enqueue_exchange(&h.transport);

        h.session.send(SessionCommand::BeginSignIn);
        wait_until(&h.session, |s| s.state == AuthState::Authenticating).await;
        h.session.send(SessionCommand::SignOut);
        wait_until(&h.session, |s| s.state == AuthState::Unauthenticated).await;
        assert_eq!(h.events.try_recv(), Ok(SessionEvent::SignedOut));

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(h.session.read().state, AuthState::Unauthenticated);
        assert!(h.events.try_recv().is_err());
        assert!(
            h.transport.pending_count() > 0,
            "the poll stopped consuming replies"
        );
    }
}
