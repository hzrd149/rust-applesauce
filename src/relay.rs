//! # Relay
//!
//! High-level Nostr relay client built on top of [`RelaySocket`].
//!
//! `Relay` is purely concerned with the Nostr protocol (NIP-01 messages,
//! subscription IDs, event routing). All WebSocket mechanics live in
//! [`RelaySocket`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rxrust::prelude::*;
use rxrust::observer::Observer as RxObserver;
use tokio::sync::oneshot;

use crate::socket::{MultiplexGuard, RelaySocket, SocketError};
use crate::types::{
    ClientMessage, CountResponse, Filter, NostrEvent, PublishResponse, ReqMessage, RelayError,
    RelayMessage,
};

// Re-export the default keepAlive so callers don't need to import socket.
pub use crate::socket::DEFAULT_KEEP_ALIVE;

// ---------------------------------------------------------------------------
// ReqHandle
// ---------------------------------------------------------------------------

/// A handle to an active Nostr subscription.
///
/// Wraps a [`MultiplexGuard`] (which owns the socket channel) and presents a
/// typed `SharedSubject<ReqMessage, RelayError>`.
///
/// Dropping the handle sends `CLOSE` to the relay and starts the socket's
/// keepAlive timer.
pub struct ReqHandle {
    /// The typed observable stream of relay messages for this subscription.
    pub subject: SharedSubject<'static, ReqMessage, RelayError>,
    /// Keeps the underlying multiplex channel alive. Drop = CLOSE + keepAlive.
    _channel: MultiplexGuard,
}

impl std::ops::Deref for ReqHandle {
    type Target = SharedSubject<'static, ReqMessage, RelayError>;
    fn deref(&self) -> &Self::Target { &self.subject }
}

impl std::ops::DerefMut for ReqHandle {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.subject }
}

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

/// A Nostr relay client.
///
/// # Lifecycle
///
/// The WebSocket is opened automatically on the first call to
/// [`req`](Relay::req), [`publish`](Relay::publish), [`count`](Relay::count),
/// or [`auth`](Relay::auth). No explicit `connect()` is required.
///
/// After the last active [`ReqHandle`] is dropped, a keepAlive timer starts.
/// If no new subscription arrives before it expires the socket closes
/// automatically.
///
/// # Clone semantics
///
/// `Relay` is cheaply `Clone`able — all clones share the same socket.
#[derive(Clone)]
pub struct Relay {
    socket: RelaySocket,
}

impl Relay {
    /// Create a new relay with the default keepAlive (30 s).
    pub fn new(url: impl Into<String>) -> Self {
        Relay { socket: RelaySocket::new(url) }
    }

    /// Create a new relay with a custom keepAlive duration.
    pub fn with_keep_alive(url: impl Into<String>, keep_alive: Duration) -> Self {
        Relay { socket: RelaySocket::with_keep_alive(url, keep_alive) }
    }

    /// Explicitly open the WebSocket (optional — operations connect
    /// automatically).
    pub async fn connect(&self) -> Result<(), RelayError> {
        self.socket.connect().await.map_err(RelayError::from)
    }

    /// Explicitly close the WebSocket.
    pub fn disconnect(&self) {
        self.socket.disconnect();
    }

    // -----------------------------------------------------------------------
    // Public reactive API
    // -----------------------------------------------------------------------

    /// Subscribe to the relay with the given filters.
    ///
    /// **Connects automatically** if not already connected. The connection is
    /// shared with all concurrent subscriptions.
    ///
    /// Returns a [`ReqHandle`] whose `.subject` field emits [`ReqMessage`]
    /// items. Dropping the handle sends `CLOSE` and starts the keepAlive.
    pub async fn req(&self, filters: Vec<Filter>) -> Result<ReqHandle, RelayError> {
        let sub_id = nanoid::nanoid!(12);
        let url = self.socket.url().to_string();

        let subscribe_msg = ClientMessage::Req {
            id: sub_id.clone(),
            filters: filters.clone(),
        }
        .to_json();
        let unsubscribe_msg = ClientMessage::Close { id: sub_id.clone() }.to_json();

        // Open a multiplexed channel; frames that contain our sub_id are forwarded.
        let sub_id_filter = sub_id.clone();
        let channel = self
            .socket
            .multiplex(subscribe_msg, unsubscribe_msg, move |text| {
                text.contains(&sub_id_filter)
            })
            .await
            .map_err(RelayError::from)?;

        // Build a typed SharedSubject on top of the raw String channel.
        let mut typed: SharedSubject<'static, ReqMessage, RelayError> = Subject::shared();
        let typed_for_task = typed.clone();

        // Emit OPEN immediately.
        RxObserver::next(typed.inner_mut(), ReqMessage::Open {
            from: url.clone(),
            id: sub_id.clone(),
            filters: filters.clone(),
        });

        // Use subscribe_raw() for the inbound bridge — rxRust's subscribe(closure)
        // requires Observer<Item, Infallible> which conflicts with our () error type.
        let mut raw_rx = self.socket.subscribe_raw().await.map_err(RelayError::from)?;

        let sub_id_task = sub_id.clone();
        let url_task = url.clone();

        tokio::spawn(async move {
            loop {
                match raw_rx.recv().await {
                    Ok(text) => {
                        // Only handle messages for our subscription.
                        if !text.contains(&sub_id_task) {
                            continue;
                        }
                        match RelayMessage::from_json(&text) {
                            Ok(RelayMessage::Event { sub_id: sid, event })
                                if sid == sub_id_task =>
                            {
                                RxObserver::next(
                                    typed_for_task.clone().inner_mut(),
                                    ReqMessage::Event {
                                        from: url_task.clone(),
                                        id: sid,
                                        event,
                                    },
                                );
                            }
                            Ok(RelayMessage::Eose { sub_id: sid }) if sid == sub_id_task => {
                                RxObserver::next(
                                    typed_for_task.clone().inner_mut(),
                                    ReqMessage::Eose {
                                        from: url_task.clone(),
                                        id: sid,
                                    },
                                );
                            }
                            Ok(RelayMessage::Closed { sub_id: sid, reason })
                                if sid == sub_id_task =>
                            {
                                RxObserver::next(
                                    typed_for_task.clone().inner_mut(),
                                    ReqMessage::Closed {
                                        from: url_task.clone(),
                                        id: sid,
                                        reason,
                                    },
                                );
                                RxObserver::complete(typed_for_task.clone().into_inner());
                                break;
                            }
                            _ => {} // not ours or irrelevant
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rx-rs-nostr/relay] req() lagged by {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        RxObserver::error(
                            typed_for_task.clone().into_inner(),
                            RelayError::ConnectionClosed,
                        );
                        break;
                    }
                }
            }
        });

        Ok(ReqHandle { subject: typed, _channel: channel })
    }

    /// Publish a Nostr event to the relay.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn publish(&self, event: NostrEvent) -> Result<PublishResponse, RelayError> {
        let event_id = event.id.clone();
        let url = self.socket.url().to_string();

        let (tx, rx) = oneshot::channel::<PublishResponse>();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;
        let event_id_task = event_id.clone();
        let url_task = url.clone();
        let tx_task = Arc::clone(&tx);

        tokio::spawn(async move {
            loop {
                match rx_msg.recv().await {
                    Ok(text) => {
                        if !text.contains(&event_id_task) { continue; }
                        if let Ok(RelayMessage::Ok { event_id: eid, ok, message }) =
                            RelayMessage::from_json(&text)
                        {
                            if eid == event_id_task {
                                if let Some(sender) = tx_task.lock().unwrap().take() {
                                    let _ = sender.send(PublishResponse {
                                        from: url_task.clone(),
                                        event_id: eid,
                                        ok,
                                        message,
                                    });
                                }
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        });

        self.socket
            .send(ClientMessage::Event(event).to_json())
            .await
            .map_err(RelayError::from)?;

        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| RelayError::WebSocket("publish timeout".into()))?
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Send a NIP-45 COUNT request.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn count(&self, filters: Vec<Filter>) -> Result<CountResponse, RelayError> {
        let sub_id = nanoid::nanoid!(12);
        let url = self.socket.url().to_string();

        let (tx, rx) = oneshot::channel::<CountResponse>();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;
        let sub_id_task = sub_id.clone();
        let url_task = url.clone();
        let tx_task = Arc::clone(&tx);

        tokio::spawn(async move {
            loop {
                match rx_msg.recv().await {
                    Ok(text) => {
                        if !text.contains(&sub_id_task) { continue; }
                        if let Ok(RelayMessage::Count { sub_id: sid, count }) =
                            RelayMessage::from_json(&text)
                        {
                            if sid == sub_id_task {
                                if let Some(sender) = tx_task.lock().unwrap().take() {
                                    let _ = sender.send(CountResponse {
                                        from: url_task.clone(),
                                        sub_id: sid,
                                        count,
                                    });
                                }
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        });

        self.socket
            .send(ClientMessage::Count { id: sub_id, filters }.to_json())
            .await
            .map_err(RelayError::from)?;

        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| RelayError::WebSocket("count timeout".into()))?
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Send a NIP-42 AUTH response event.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn auth(&self, event: NostrEvent) -> Result<PublishResponse, RelayError> {
        let event_id = event.id.clone();
        let url = self.socket.url().to_string();

        let (tx, rx) = oneshot::channel::<PublishResponse>();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;
        let event_id_task = event_id.clone();
        let url_task = url.clone();
        let tx_task = Arc::clone(&tx);

        tokio::spawn(async move {
            loop {
                match rx_msg.recv().await {
                    Ok(text) => {
                        if !text.contains(&event_id_task) { continue; }
                        if let Ok(RelayMessage::Ok { event_id: eid, ok, message }) =
                            RelayMessage::from_json(&text)
                        {
                            if eid == event_id_task {
                                if let Some(sender) = tx_task.lock().unwrap().take() {
                                    let _ = sender.send(PublishResponse {
                                        from: url_task.clone(),
                                        event_id: eid,
                                        ok,
                                        message,
                                    });
                                }
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        });

        self.socket
            .send(ClientMessage::Auth(event).to_json())
            .await
            .map_err(RelayError::from)?;

        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| RelayError::WebSocket("auth timeout".into()))?
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Expose the parsed relay message stream.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn messages(
        &self,
    ) -> Result<SharedSubject<'static, RelayMessage, ()>, RelayError> {
        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;

        let subject: SharedSubject<'static, RelayMessage, ()> = Subject::shared();
        let subject_for_task = subject.clone();

        tokio::spawn(async move {
            loop {
                match rx_msg.recv().await {
                    Ok(text) => match RelayMessage::from_json(&text) {
                        Ok(msg) => {
                            RxObserver::next(subject_for_task.clone().inner_mut(), msg);
                        }
                        Err(e) => {
                            eprintln!("[rx-rs-nostr/relay] parse error: {e}");
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rx-rs-nostr/relay] messages() lagged by {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        RxObserver::error(subject_for_task.clone().into_inner(), ());
                        break;
                    }
                }
            }
        });

        Ok(subject)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    pub fn url(&self) -> &str { self.socket.url() }

    pub fn is_connected(&self) -> bool { self.socket.is_connected() }

    pub fn subscription_count(&self) -> usize { self.socket.channel_count() }

    /// Access the underlying [`RelaySocket`] directly.
    pub fn socket(&self) -> &RelaySocket { &self.socket }
}

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

impl From<SocketError> for RelayError {
    fn from(e: SocketError) -> Self {
        match e {
            SocketError::Connect(msg) => RelayError::WebSocket(msg),
            SocketError::Closed => RelayError::ConnectionClosed,
        }
    }
}
