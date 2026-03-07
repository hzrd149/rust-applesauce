//! # RelaySocket
//!
//! A low-level WebSocket abstraction modelled after RxJS's `WebSocketSubject`.
//!
//! `RelaySocket` owns the connection lifecycle and exposes three building
//! blocks:
//!
//! - [`RelaySocket::send`]      — write a raw text frame
//! - [`RelaySocket::messages`]  — observe every inbound text frame as a Stream
//! - [`RelaySocket::multiplex`] — open a logical channel over the shared
//!                                socket, returning an inbound [`Stream`] and
//!                                an outbound [`SocketSender`]
//!
//! Higher-level code (e.g. [`Relay`](crate::relay::Relay)) never touches
//! WebSocket primitives directly.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_stream::stream;
use futures_util::{SinkExt, Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Default keepAlive — how long the socket stays open after the last
/// multiplexed channel is dropped.
pub const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Internal actor command
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum Command {
    Send(String),
    Disconnect,
}

// ---------------------------------------------------------------------------
// ConnectionHandle — live connection state (crate-private)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct ConnectionHandle {
    pub(crate) cmd_tx: mpsc::UnboundedSender<Command>,
    /// Broadcast fan-out of every inbound text frame.
    pub(crate) msg_tx: broadcast::Sender<String>,
}

// ---------------------------------------------------------------------------
// SocketSender — outbound half of a multiplexed channel
// ---------------------------------------------------------------------------

/// The outbound half returned by [`RelaySocket::multiplex`].
///
/// Call [`send`](SocketSender::send) to write raw text frames to the relay
/// over the shared WebSocket connection.
///
/// Cheap to clone — all clones write to the same socket.
#[derive(Clone)]
pub struct SocketSender {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl SocketSender {
    /// Send a raw text frame to the relay.
    pub fn send(&self, text: String) -> Result<(), SocketError> {
        self.cmd_tx
            .send(Command::Send(text))
            .map_err(|_| SocketError::Closed)
    }
}

// ---------------------------------------------------------------------------
// DropGuard — RAII teardown for a multiplexed channel
// ---------------------------------------------------------------------------

/// Sends the unsubscribe message and decrements the ref-count when dropped.
/// Captured inside the `stream!` block so it fires on any drop path —
/// whether the stream is fully consumed or abandoned early.
struct DropGuard {
    cmd_tx: mpsc::UnboundedSender<Command>,
    unsubscribe_msg: String,
    sub_count: Arc<AtomicUsize>,
    socket: RelaySocket,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Send(self.unsubscribe_msg.clone()));
        let prev = self.sub_count.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.socket.schedule_keep_alive();
        }
    }
}

// ---------------------------------------------------------------------------
// RelaySocket
// ---------------------------------------------------------------------------

/// A lazy, reference-counted WebSocket connection.
///
/// - Opens on first use — no explicit `connect()` needed.
/// - All [`multiplex`](RelaySocket::multiplex) channels share one socket.
/// - After the last channel drops, a keepAlive timer starts; if no new
///   channel opens before it expires, the socket closes.
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
                                eprintln!("[rust-applesauce/socket] ws error: {e}");
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

    /// Schedule a keepAlive check. Called when a channel's ref-count hits 0.
    pub(crate) fn schedule_keep_alive(&self) {
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

    /// Send a raw text frame to the relay.
    ///
    /// Connects automatically if not already connected.
    pub async fn send(&self, text: String) -> Result<(), SocketError> {
        let handle = self.ensure_connected().await?;
        handle
            .cmd_tx
            .send(Command::Send(text))
            .map_err(|_| SocketError::Closed)
    }

    /// Subscribe to every inbound text frame as a `Stream<Item = String>`.
    ///
    /// Connects automatically if not already connected.
    pub async fn messages(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = String> + Send + 'static>>, SocketError> {
        let mut rx = self.ensure_connected().await?.msg_tx.subscribe();

        Ok(Box::pin(stream! {
            loop {
                match rx.recv().await {
                    Ok(text) => yield text,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rust-applesauce/socket] messages() lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }))
    }

    /// Subscribe to every inbound text frame as a raw `broadcast::Receiver`.
    ///
    /// Used internally by `Relay` for one-shot request/response operations
    /// (publish, count, auth) where a lightweight receiver is enough.
    pub(crate) async fn subscribe_raw(
        &self,
    ) -> Result<broadcast::Receiver<String>, SocketError> {
        Ok(self.ensure_connected().await?.msg_tx.subscribe())
    }

    /// Open a multiplexed logical channel over the shared socket.
    ///
    /// Analogous to RxJS `WebSocketSubject.multiplex()`:
    ///
    /// - `subscribe_msg`   is sent immediately when the channel opens.
    /// - `unsubscribe_msg` is sent when the returned stream is dropped.
    /// - `filter`          selects which inbound frames belong to this channel.
    ///
    /// Returns a `(stream, sender)` pair:
    ///
    /// - **`stream`** — `Pin<Box<dyn Stream<Item = String>>>` yielding every
    ///   inbound frame that passes `filter`. Dropping the stream sends the
    ///   unsubscribe message and starts the keepAlive timer.
    /// - **`sender`** — [`SocketSender`] for writing frames to the relay over
    ///   this channel's shared connection.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use rust_applesauce::RelaySocket;
    /// # use futures_util::StreamExt;
    /// # #[tokio::main] async fn main() {
    /// let socket = RelaySocket::new("wss://relay.damus.io");
    ///
    /// let (mut stream, sender) = socket
    ///     .multiplex(
    ///         r#"["REQ","sub1",{"kinds":[1],"limit":5}]"#.into(),
    ///         r#"["CLOSE","sub1"]"#.into(),
    ///         |text| text.contains("sub1"),
    ///     )
    ///     .await
    ///     .unwrap();
    ///
    /// while let Some(frame) = stream.next().await {
    ///     println!("{frame}");
    /// }
    /// # }
    /// ```
    pub async fn multiplex(
        &self,
        subscribe_msg: String,
        unsubscribe_msg: String,
        filter: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Result<(Pin<Box<dyn Stream<Item = String> + Send + 'static>>, SocketSender), SocketError>
    {
        let handle = self.ensure_connected().await?;

        // Increment ref-count before sending the subscribe message.
        self.sub_count.fetch_add(1, Ordering::SeqCst);

        if handle.cmd_tx.send(Command::Send(subscribe_msg)).is_err() {
            self.sub_count.fetch_sub(1, Ordering::SeqCst);
            return Err(SocketError::Closed);
        }

        let mut rx = handle.msg_tx.subscribe();
        let filter = Arc::new(filter);

        // RAII guard — fires teardown (CLOSE + keepAlive) when dropped,
        // regardless of whether the stream was fully consumed or dropped early.
        let guard = DropGuard {
            cmd_tx: handle.cmd_tx.clone(),
            unsubscribe_msg,
            sub_count: Arc::clone(&self.sub_count),
            socket: self.clone(),
        };

        let inbound = Box::pin(stream! {
            // Hold the guard alive for the lifetime of the stream.
            let _guard = guard;

            loop {
                match rx.recv().await {
                    Ok(text) => {
                        if filter(&text) {
                            yield text;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rust-applesauce/socket] multiplex lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        let sender = SocketSender { cmd_tx: handle.cmd_tx.clone() };

        Ok((inbound, sender))
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
