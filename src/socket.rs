//! # RelaySocket
//!
//! A low-level WebSocket abstraction modelled after RxJS's `WebSocketSubject`.
//!
//! `RelaySocket` owns the connection lifecycle — opening, closing, keepAlive —
//! and exposes three building blocks:
//!
//! - [`RelaySocket::send`]      — write a raw text frame
//! - [`RelaySocket::messages`]  — observe every inbound text frame
//! - [`RelaySocket::multiplex`] — demultiplex one logical channel over the
//!                                shared socket, with automatic subscribe /
//!                                unsubscribe on create / drop
//!
//! Higher-level code (e.g. the Nostr [`Relay`](crate::relay::Relay)) never
//! touches WebSocket primitives directly.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt as WsStreamExt;
use rxrust::prelude::*;
use rxrust::observer::Observer as RxObserver;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Default keepAlive duration — how long the socket stays open after the last
/// multiplexed channel is dropped.
pub const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Internal actor command
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Command {
    Send(String),
    Disconnect,
}

// ---------------------------------------------------------------------------
// ConnectionHandle — live connection state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct ConnectionHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Broadcast fan-out of every inbound text frame.
    msg_tx: broadcast::Sender<String>,
}

// ---------------------------------------------------------------------------
// MultiplexGuard — RAII unsubscribe for a single channel
// ---------------------------------------------------------------------------

/// Returned by [`RelaySocket::multiplex`]. Dropping this sends the
/// unsubscribe message and decrements the socket's reference count, which
/// may trigger the keepAlive timer.
pub struct MultiplexGuard {
    /// The inbound message stream for this channel.
    pub subject: SharedSubject<'static, String, ()>,
    _inner: Arc<MultiplexGuardInner>,
}

struct MultiplexGuardInner {
    unsubscribe_msg: String,
    cmd_tx: mpsc::UnboundedSender<Command>,
    sub_count: Arc<AtomicUsize>,
    socket: RelaySocket,
}

impl Drop for MultiplexGuardInner {
    fn drop(&mut self) {
        // Send the unsubscribe message (e.g. `["CLOSE","<id>"]`).
        let _ = self.cmd_tx.send(Command::Send(self.unsubscribe_msg.clone()));

        // Decrement ref-count; start keepAlive if this was the last channel.
        let prev = self.sub_count.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.socket.schedule_keep_alive();
        }
    }
}

impl std::ops::Deref for MultiplexGuard {
    type Target = SharedSubject<'static, String, ()>;
    fn deref(&self) -> &Self::Target { &self.subject }
}

impl std::ops::DerefMut for MultiplexGuard {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.subject }
}

// ---------------------------------------------------------------------------
// RelaySocket
// ---------------------------------------------------------------------------

/// A lazy, reference-counted WebSocket connection.
///
/// Modelled after RxJS's `WebSocketSubject`:
///
/// - The socket opens on the first [`multiplex`](RelaySocket::multiplex) /
///   [`send`](RelaySocket::send) call; no explicit `connect()` needed.
/// - All [`multiplex`](RelaySocket::multiplex) channels share one socket.
/// - After the last channel is dropped, a keepAlive timer starts; if no new
///   channel is opened before it expires, the socket closes.
/// - [`connect`](RelaySocket::connect) / [`disconnect`](RelaySocket::disconnect)
///   are available for explicit lifecycle control.
///
/// # Clone semantics
///
/// `RelaySocket` is cheaply `Clone`able — all clones share the same underlying
/// connection, ref-count, and keepAlive state.
#[derive(Clone)]
pub struct RelaySocket {
    url: String,
    handle: Arc<Mutex<Option<ConnectionHandle>>>,
    sub_count: Arc<AtomicUsize>,
    keep_alive: Duration,
}

impl RelaySocket {
    /// Create a new socket handle with the default keepAlive (30 s).
    /// The WebSocket is **not** opened yet.
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_keep_alive(url, DEFAULT_KEEP_ALIVE)
    }

    /// Create a new socket handle with a custom keepAlive duration.
    pub fn with_keep_alive(url: impl Into<String>, keep_alive: Duration) -> Self {
        RelaySocket {
            url: url.into(),
            handle: Arc::new(Mutex::new(None)),
            sub_count: Arc::new(AtomicUsize::new(0)),
            keep_alive,
        }
    }

    // -----------------------------------------------------------------------
    // Connection lifecycle
    // -----------------------------------------------------------------------

    /// Ensure the WebSocket is open, connecting if necessary.
    ///
    /// Safe to call concurrently — only one connection is ever opened.
    pub(crate) async fn ensure_connected(&self) -> Result<ConnectionHandle, SocketError> {
        // Fast path.
        {
            let guard = self.handle.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                return Ok(h.clone());
            }
        }

        // Slow path — open the socket.
        let (ws_stream, _) = connect_async(&self.url)
            .await
            .map_err(|e| SocketError::Connect(e.to_string()))?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (msg_tx, _) = broadcast::channel::<String>(1024);
        let msg_tx_actor = msg_tx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = ws_source.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let _ = msg_tx_actor.send(text.to_string());
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                eprintln!("[rx-rs-nostr/socket] ws error: {e}");
                                break;
                            }
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(Command::Send(text)) => {
                                if ws_sink.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            Some(Command::Disconnect) | None => {
                                let _ = ws_sink.close().await;
                                break;
                            }
                        }
                    }
                }
            }
        });

        let handle = ConnectionHandle { cmd_tx, msg_tx };

        // Double-check: another task may have connected while we awaited.
        let mut guard = self.handle.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        *guard = Some(handle.clone());
        Ok(handle)
    }

    /// Explicitly open the socket (optional — operations connect automatically).
    pub async fn connect(&self) -> Result<(), SocketError> {
        self.ensure_connected().await.map(|_| ())
    }

    /// Explicitly close the socket.
    pub fn disconnect(&self) {
        let mut guard = self.handle.lock().unwrap();
        if let Some(h) = guard.as_ref() {
            let _ = h.cmd_tx.send(Command::Disconnect);
        }
        *guard = None;
        self.sub_count.store(0, Ordering::SeqCst);
    }

    /// Schedule a keepAlive check after `self.keep_alive`. If no channels are
    /// active when the timer fires, the socket is closed.
    fn schedule_keep_alive(&self) {
        let handle_arc = Arc::clone(&self.handle);
        let sub_count = Arc::clone(&self.sub_count);
        let keep_alive = self.keep_alive;

        tokio::spawn(async move {
            tokio::time::sleep(keep_alive).await;
            if sub_count.load(Ordering::SeqCst) == 0 {
                let mut guard = handle_arc.lock().unwrap();
                if let Some(h) = guard.as_ref() {
                    let _ = h.cmd_tx.send(Command::Disconnect);
                }
                *guard = None;
            }
        });
    }

    // -----------------------------------------------------------------------
    // Public I/O surface
    // -----------------------------------------------------------------------

    /// Send a raw text frame to the remote end.
    ///
    /// Connects automatically if not already connected.
    pub async fn send(&self, text: String) -> Result<(), SocketError> {
        let handle = self.ensure_connected().await?;
        handle
            .cmd_tx
            .send(Command::Send(text))
            .map_err(|_| SocketError::Closed)
    }

    /// Send a raw text frame using an already-obtained handle.
    ///
    /// This is the sync variant used internally when a handle is already in
    /// hand (avoids double-locking in hot paths).
    pub(crate) fn send_with_handle(
        handle: &ConnectionHandle,
        text: String,
    ) -> Result<(), SocketError> {
        handle
            .cmd_tx
            .send(Command::Send(text))
            .map_err(|_| SocketError::Closed)
    }

    /// Subscribe to every inbound text frame as an rxRust `SharedSubject`.
    ///
    /// Connects automatically if not already connected.
    pub async fn messages(&self) -> Result<SharedSubject<'static, String, ()>, SocketError> {
        let handle = self.ensure_connected().await?;
        let subject: SharedSubject<'static, String, ()> = Subject::shared();
        let subject_for_task = subject.clone();
        let mut rx = handle.msg_tx.subscribe();

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(text) => {
                        RxObserver::next(subject_for_task.clone().inner_mut(), text);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rx-rs-nostr/socket] messages() lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        RxObserver::error(subject_for_task.clone().into_inner(), ());
                        break;
                    }
                }
            }
        });

        Ok(subject)
    }

    /// Subscribe to every inbound text frame as a raw `broadcast::Receiver`.
    ///
    /// This is the low-overhead variant used internally by [`Relay`] to avoid
    /// the overhead of an extra rxRust layer for one-shot operations.
    pub async fn subscribe_raw(&self) -> Result<broadcast::Receiver<String>, SocketError> {
        let handle = self.ensure_connected().await?;
        Ok(handle.msg_tx.subscribe())
    }

    /// Open a multiplexed logical channel over the shared socket.
    ///
    /// Analogous to RxJS `WebSocketSubject.multiplex()`:
    ///
    /// - `subscribe_msg`   is sent to the remote end immediately.
    /// - `unsubscribe_msg` is sent when the returned [`MultiplexGuard`] is
    ///                     dropped.
    /// - `filter`          is called with each inbound frame; only frames for
    ///                     which it returns `true` are forwarded to the
    ///                     guard's subject.
    ///
    /// The returned [`MultiplexGuard`] holds an rxRust `SharedSubject<String, ()>`
    /// (accessible via `.subject` or `Deref`). Dropping the guard sends the
    /// unsubscribe message and starts the keepAlive timer if this was the last
    /// active channel.
    pub async fn multiplex(
        &self,
        subscribe_msg: String,
        unsubscribe_msg: String,
        filter: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Result<MultiplexGuard, SocketError> {
        let handle = self.ensure_connected().await?;

        // Increment ref-count before sending the subscribe message.
        self.sub_count.fetch_add(1, Ordering::SeqCst);

        if let Err(e) = Self::send_with_handle(&handle, subscribe_msg) {
            self.sub_count.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }

        let subject: SharedSubject<'static, String, ()> = Subject::shared();
        let subject_for_task = subject.clone();
        let mut rx = handle.msg_tx.subscribe();
        let filter = Arc::new(filter);

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(text) => {
                        if filter(&text) {
                            RxObserver::next(subject_for_task.clone().inner_mut(), text);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rx-rs-nostr/socket] multiplex lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        RxObserver::error(subject_for_task.clone().into_inner(), ());
                        break;
                    }
                }
            }
        });

        let guard = Arc::new(MultiplexGuardInner {
            unsubscribe_msg,
            cmd_tx: handle.cmd_tx.clone(),
            sub_count: Arc::clone(&self.sub_count),
            socket: self.clone(),
        });

        Ok(MultiplexGuard { subject, _inner: guard })
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    pub fn url(&self) -> &str { &self.url }

    pub fn is_connected(&self) -> bool {
        self.handle.lock().unwrap().is_some()
    }

    pub fn channel_count(&self) -> usize {
        self.sub_count.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, thiserror::Error)]
pub enum SocketError {
    #[error("WebSocket connect error: {0}")]
    Connect(String),
    #[error("WebSocket connection is closed")]
    Closed,
}
