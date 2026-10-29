//! Port of qt/src/xcloud/StreamSession.{h,cpp}.
//!
//! Owns the provisioning state machine, signaling exchange and keepalive for
//! one xCloud/xHome session. Mirrors the Swift `StreamSession`.
//!
//! Differences from the C++:
//! - `stateChanged` is a `tokio::sync::watch` channel ([`StreamSession::subscribe`]);
//!   `intermediateState` is an optional `mpsc::UnboundedSender<IntermediateState>`
//!   installed with [`StreamSession::set_intermediate_state_sender`].
//! - The `QTimer` keepalive is a tokio task whose handle the session owns;
//!   [`StreamSession::stop_keepalive`], [`StreamSession::stop`] and `Drop` abort it.
//! - `waitForStates` sleeps with `tokio::time::sleep`; the interval and timeout
//!   are `Duration`s so tests run with a 1 ms poll.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use xcloud_proto::models::{IceCandidatePayload, StreamSessionStartResponse};

use crate::client::{PollPolicy, XCloudApiClient};
use crate::{Error, Result};

const LOG: &str = "StreamSession";

/// `waitForStates(pollIntervalMs = 1000)`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// `waitForStates(timeoutMs = 60000)`.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_millis(60_000);
/// `startKeepalive(intervalSeconds = 30)`.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// `StreamSession::State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SessionState {
    #[default]
    New,
    Provisioning,
    Provisioned,
    ReadyToConnect,
    Error,
}

/// `streamSessionStateName`.
pub fn session_state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::New => "New",
        SessionState::Provisioning => "Provisioning",
        SessionState::Provisioned => "Provisioned",
        SessionState::ReadyToConnect => "ReadyToConnect",
        SessionState::Error => "Error",
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(session_state_name(*self))
    }
}

/// The `intermediateState(state, estimatedWaitSeconds)` signal: emitted for
/// non-terminal provisioning states such as `WaitingForResources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntermediateState {
    pub state: String,
    pub estimated_wait_seconds: i32,
}

/// Lifecycle for a single xCloud/xHome streaming session.
pub struct StreamSession {
    api: Arc<XCloudApiClient>,
    session_id: String,
    session_path: String,
    state: watch::Sender<SessionState>,
    intermediate: Mutex<Option<mpsc::UnboundedSender<IntermediateState>>>,
    keepalive: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for StreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSession")
            .field("session_id", &self.session_id)
            .field("session_path", &self.session_path)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl StreamSession {
    /// Builds the session from a `/play` response. An empty `sessionId` gets a
    /// fresh UUID; the path is made absolute (`/v5/sessions/cloud/ABCD`).
    pub fn new(api: Arc<XCloudApiClient>, response: &StreamSessionStartResponse) -> Self {
        let session_id = if response.session_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            response.session_id.clone()
        };
        let session_path = if response.session_path.starts_with('/') {
            response.session_path.clone()
        } else {
            format!("/{}", response.session_path)
        };
        Self {
            api,
            session_id,
            session_path,
            state: watch::Sender::new(SessionState::New),
            intermediate: Mutex::new(None),
            keepalive: Mutex::new(None),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Absolute session path, e.g. "/v5/sessions/cloud/ABCD".
    pub fn session_path(&self) -> &str {
        &self.session_path
    }

    pub fn api(&self) -> &Arc<XCloudApiClient> {
        &self.api
    }

    pub fn state(&self) -> SessionState {
        *self.state.borrow()
    }

    /// The `stateChanged` signal: the receiver wakes on every state change and
    /// `borrow()`s the latest state.
    pub fn subscribe(&self) -> watch::Receiver<SessionState> {
        self.state.subscribe()
    }

    /// Installs the `intermediateState` signal sink.
    pub fn set_intermediate_state_sender(&self, sender: mpsc::UnboundedSender<IntermediateState>) {
        *lock(&self.intermediate) = Some(sender);
    }

    fn set_state(&self, state: SessionState) {
        self.state.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
    }

    fn emit_intermediate(&self, state: &str, estimated_wait_seconds: i32) {
        let mut slot = lock(&self.intermediate);
        if let Some(sender) = slot.as_ref()
            && sender
                .send(IntermediateState {
                    state: state.to_owned(),
                    estimated_wait_seconds,
                })
                .is_err()
        {
            // The receiver is gone; stop reporting.
            *slot = None;
        }
    }

    /// `waitForStates`: polls the state endpoint until one of `target_states`
    /// is observed or `timeout` elapses. `title_id`, when non-empty, enables
    /// the wait-time lookup reported through the intermediate-state channel
    /// while the service is `WaitingForResources`.
    pub async fn wait_for_states(
        &self,
        target_states: &[&str],
        poll_interval: Duration,
        timeout: Duration,
        title_id: &str,
    ) -> Result<SessionState> {
        let deadline = Instant::now() + timeout.max(Duration::from_millis(1));
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "Timed out waiting for {}",
                    target_states.join(", ")
                )));
            }

            let result = self
                .api
                .get_session_state(&self.session_path)
                .await
                .map_err(|error| Error::Other(format!("State poll error: {error}")))?;

            let state = result.state.as_str();
            tracing::info!(target: LOG, "Session state: {state}");

            if target_states.contains(&state) {
                match state {
                    "ReadyToConnect" => self.set_state(SessionState::ReadyToConnect),
                    "Provisioned" => self.set_state(SessionState::Provisioned),
                    "Provisioning" | "WaitingForResources" => {
                        self.set_state(SessionState::Provisioning)
                    }
                    _ => {}
                }
                return Ok(self.state());
            }

            match state {
                "Provisioned" => self.set_state(SessionState::Provisioned),
                "Provisioning" | "WaitingForResources" => {
                    self.set_state(SessionState::Provisioning);
                    if state == "WaitingForResources" && !title_id.is_empty() {
                        let wait = self.api.get_wait_time(&self.session_path, title_id).await;
                        self.emit_intermediate(
                            "WaitingForResources",
                            wait.map(|w| w.estimated_total_wait_time_in_seconds)
                                .unwrap_or(0),
                        );
                    }
                }
                _ if result.has_error() => {
                    self.set_state(SessionState::Error);
                    return Err(Error::Other(format!(
                        "{}: {}",
                        result.error_code, result.error_message
                    )));
                }
                _ => {
                    tracing::warn!(target: LOG, "Unknown session state: {state}");
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    pub async fn wait_until_ready(
        &self,
        poll_interval: Duration,
        timeout: Duration,
    ) -> Result<SessionState> {
        self.wait_for_states(&["ReadyToConnect"], poll_interval, timeout, "")
            .await
    }

    pub async fn wait_until_ready_or_provisioned(
        &self,
        poll_interval: Duration,
        timeout: Duration,
        title_id: &str,
    ) -> Result<SessionState> {
        self.wait_for_states(
            &["ReadyToConnect", "Provisioned"],
            poll_interval,
            timeout,
            title_id,
        )
        .await
    }

    pub async fn wait_until_provisioned(
        &self,
        poll_interval: Duration,
        timeout: Duration,
        title_id: &str,
    ) -> Result<SessionState> {
        self.wait_for_states(&["Provisioned"], poll_interval, timeout, title_id)
            .await
    }

    /// `exchangeSdp`: posts the local offer and returns the remote answer.
    pub async fn exchange_sdp(&self, local_sdp: &str) -> Result<String> {
        self.api
            .send_sdp_offer(&self.session_path, local_sdp, PollPolicy::SDP)
            .await
    }

    /// `exchangeIce`: posts the local candidates and returns the remote ones.
    pub async fn exchange_ice(
        &self,
        local_candidates: &[IceCandidatePayload],
        prefer_ipv6: bool,
    ) -> Result<Vec<IceCandidatePayload>> {
        self.api
            .send_ice_candidates(
                &self.session_path,
                local_candidates,
                prefer_ipv6,
                PollPolicy::ICE,
            )
            .await
    }

    pub async fn send_msal_auth(&self, user_token: &str) -> Result<()> {
        self.api
            .send_msal_auth(&self.session_path, user_token)
            .await
    }

    /// `startKeepalive`: posts `/keepalive` every `interval` (at least one
    /// second) on a tokio task until stopped. Must be called from within a
    /// tokio runtime. Replaces any running keepalive.
    pub fn start_keepalive(&self, interval: Duration) {
        self.stop_keepalive();
        let interval = interval.max(Duration::from_secs(1));
        let api = self.api.clone();
        let session_path = self.session_path.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(error) = api.send_keepalive(&session_path).await {
                    tracing::debug!(target: LOG, "keepalive failed: {error}");
                }
            }
        });
        *lock(&self.keepalive) = Some(handle);
    }

    pub fn stop_keepalive(&self) {
        if let Some(handle) = lock(&self.keepalive).take() {
            handle.abort();
        }
    }

    /// True while a keepalive task is running.
    pub fn keepalive_running(&self) -> bool {
        lock(&self.keepalive)
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// `stop`: stops the keepalive, resets to `New` and deletes the session.
    pub async fn stop(&self) -> Result<()> {
        self.stop_keepalive();
        self.set_state(SessionState::New);
        self.api.stop_session(&self.session_path).await
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stop_keepalive();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_streamsession.cpp.
    use std::sync::Arc;

    use super::*;
    use crate::transport::{FakeTransport, Reply};

    fn client(transport: &Arc<FakeTransport>) -> Arc<XCloudApiClient> {
        let client = XCloudApiClient::new(transport.clone());
        client.update_credentials("https://host", "GS");
        Arc::new(client)
    }

    fn response(session_id: &str) -> StreamSessionStartResponse {
        StreamSessionStartResponse {
            session_path: "v5/sessions/cloud/ABCD".to_owned(),
            session_id: session_id.to_owned(),
            state: String::new(),
        }
    }

    const FAST: Duration = Duration::from_millis(1);
    const TWO_SECONDS: Duration = Duration::from_millis(2000);

    #[test]
    fn normalizes_session_path() {
        let transport = Arc::new(FakeTransport::new());
        let client = Arc::new(XCloudApiClient::new(transport));
        let session = StreamSession::new(client, &response("ABCD"));
        assert_eq!(session.session_path(), "/v5/sessions/cloud/ABCD");
        assert_eq!(session.session_id(), "ABCD");
        assert_eq!(session.state(), SessionState::New);
    }

    #[tokio::test]
    async fn waits_for_ready_state() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        transport.enqueue_json("/state", r#"{"state":"Provisioning"}"#, 200);
        transport.enqueue_json("/state", r#"{"state":"ReadyToConnect"}"#, 200);

        let session = StreamSession::new(client, &response(""));

        let spy = session.subscribe();
        let result = session
            .wait_for_states(&["ReadyToConnect"], FAST, TWO_SECONDS, "")
            .await
            .unwrap();
        assert_eq!(result, SessionState::ReadyToConnect);
        assert_eq!(session.state(), SessionState::ReadyToConnect);
        assert!(spy.has_changed().unwrap());
    }

    #[tokio::test]
    async fn surfaces_server_error() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        transport.enqueue_json(
            "/state",
            r#"{"state":"Failed","errorDetails":{"code":"ProvisioningError","message":"no console"}}"#,
            200,
        );

        let session = StreamSession::new(client, &response(""));

        let err = session
            .wait_for_states(&["ReadyToConnect"], FAST, TWO_SECONDS, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ProvisioningError"), "{err}");
        assert_eq!(session.state(), SessionState::Error);
    }

    // -- Rust-side coverage of behaviour the C++ exercised only end to end. --

    #[test]
    fn empty_session_id_gets_a_uuid() {
        let transport = Arc::new(FakeTransport::new());
        let client = Arc::new(XCloudApiClient::new(transport));
        let session = StreamSession::new(client, &response(""));
        assert!(uuid::Uuid::parse_str(session.session_id()).is_ok());
        assert_eq!(session_state_name(session.state()), "New");
    }

    #[tokio::test]
    async fn reports_waiting_for_resources_with_wait_time() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json("/state", r#"{"state":"WaitingForResources"}"#, 200);
        transport.enqueue_json(
            "/waittime/TITLE",
            r#"{"estimatedTotalWaitTimeInSeconds":42}"#,
            200,
        );
        transport.enqueue_json("/state", r#"{"state":"Provisioned"}"#, 200);

        let session = StreamSession::new(client, &response(""));
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.set_intermediate_state_sender(tx);

        let result = session
            .wait_until_ready_or_provisioned(FAST, TWO_SECONDS, "TITLE")
            .await
            .unwrap();
        assert_eq!(result, SessionState::Provisioned);
        assert_eq!(
            rx.try_recv().unwrap(),
            IntermediateState {
                state: "WaitingForResources".to_owned(),
                estimated_wait_seconds: 42,
            }
        );
    }

    #[tokio::test]
    async fn times_out_when_target_never_arrives() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        for _ in 0..50 {
            transport.enqueue_json("/state", r#"{"state":"Provisioning"}"#, 200);
        }
        let session = StreamSession::new(client, &response(""));
        let err = session
            .wait_until_ready(FAST, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        assert_eq!(session.state(), SessionState::Provisioning);
    }

    #[tokio::test]
    async fn poll_failure_is_reported() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_error("/state", "boom");
        let session = StreamSession::new(client, &response(""));
        let err = session
            .wait_until_provisioned(FAST, TWO_SECONDS, "")
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("State poll error: "), "{err}");
    }

    /// A malformed 200 from `/state` is an empty `StreamStateResponse` in the
    /// C++ (`QJsonDocument::fromJson(..).object()`), logged as
    /// `Unknown session state:` and polled past, never a fatal poll error.
    #[tokio::test]
    async fn malformed_state_body_keeps_polling() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue(
            "/state",
            Reply::new(200, b"<html>Service Unavailable</html>".to_vec()),
        );
        transport.enqueue("/state", Reply::new(200, b"{".to_vec()));
        transport.enqueue_json("/state", r#"{"state":"ReadyToConnect"}"#, 200);

        let session = StreamSession::new(client, &response(""));
        let result = session.wait_until_ready(FAST, TWO_SECONDS).await.unwrap();
        assert_eq!(result, SessionState::ReadyToConnect);
        assert_eq!(
            transport
                .requests()
                .iter()
                .filter(|request| request.url.ends_with("/state"))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn malformed_state_body_only_times_out() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        for _ in 0..50 {
            transport.enqueue("/state", Reply::new(200, b"{".to_vec()));
        }
        let session = StreamSession::new(client, &response(""));
        let err = session
            .wait_until_ready(FAST, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        assert_eq!(session.state(), SessionState::New);
    }

    #[tokio::test]
    async fn stop_resets_state_and_deletes_session() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json("/state", r#"{"state":"ReadyToConnect"}"#, 200);
        transport.enqueue_for(
            "DELETE",
            "/v5/sessions/cloud/ABCD",
            Reply::new(204, Vec::new()),
        );

        let session = StreamSession::new(client, &response(""));
        session.wait_until_ready(FAST, TWO_SECONDS).await.unwrap();
        session.start_keepalive(Duration::from_secs(30));
        assert!(session.keepalive_running());

        session.stop().await.unwrap();
        assert_eq!(session.state(), SessionState::New);
        assert!(!session.keepalive_running());
        assert!(transport.requests().iter().any(|request| {
            request.method == "DELETE" && request.url.ends_with("/v5/sessions/cloud/ABCD")
        }));
    }

    #[tokio::test]
    async fn exchange_methods_target_session_path() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json("/v5/sessions/cloud/ABCD/sdp", r#"{"sdp":"ANSWER"}"#, 200);
        transport.enqueue(
            "/v5/sessions/cloud/ABCD/connect",
            Reply::new(200, Vec::new()),
        );

        let session = StreamSession::new(client, &response(""));
        assert_eq!(session.exchange_sdp("v=0").await.unwrap(), "ANSWER");
        session.send_msal_auth("TOKEN").await.unwrap();
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].body, br#"{"userToken":"TOKEN"}"#);
    }
}
