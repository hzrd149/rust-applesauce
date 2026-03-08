//! # Relay
//!
//! High-level Nostr relay client built on top of [`RelaySocket`].
//!
//! `Relay` is purely concerned with the Nostr protocol (NIP-01 messages,
//! subscription IDs, event routing). All WebSocket mechanics live in
//! [`RelaySocket`].

use std::pin::Pin;
use std::time::Duration;

use async_stream::stream;
use futures_util::{Stream, StreamExt};
use tokio::sync::oneshot;

use crate::socket::{RelaySocket, SocketError};
use crate::types::{
    ClientMessage, CountResponse, Filter, NostrEvent, PublishResponse, ReqMessage, RelayError,
    RelayMessage,
};

pub use crate::socket::DEFAULT_KEEP_ALIVE;

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

/// A Nostr relay client.
///
/// # Lazy connection
///
/// The WebSocket opens automatically the first time a stream returned by
/// [`req`](Relay::req) is polled, or when [`publish`](Relay::publish) /
/// [`count`](Relay::count) / [`auth`](Relay::auth) are awaited. No explicit
/// `connect()` call is needed.
///
/// # Subscriptions and keepAlive
///
/// [`req`](Relay::req) returns a cold [`Stream`] synchronously. On first poll
/// the REQ is sent and events start flowing. Dropping the stream sends `CLOSE`
/// to the relay. After the last active stream is dropped a keepAlive timer
/// starts; if no new stream is polled before it expires the socket closes.
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
    /// Returns a cold [`Stream`] synchronously — nothing happens until the
    /// stream is first polled. On first poll:
    ///
    /// 1. The WebSocket connects (if not already open).
    /// 2. A `REQ` message is sent with a generated subscription ID.
    /// 3. Items begin flowing.
    ///
    /// Stream items are `Result<ReqMessage, RelayError>`:
    ///
    /// - `Ok(Open { .. })`   — subscription registered; REQ was accepted.
    /// - `Ok(Event { .. })`  — a matching event arrived.
    /// - `Ok(Eose { .. })`   — end of stored events; live feed continues.
    /// - `Ok(Closed { .. })` — relay closed the subscription; stream ends.
    /// - `Err(_)`            — connection error; stream ends.
    ///
    /// Dropping the stream before it ends sends `CLOSE` to the relay and
    /// starts the keepAlive timer.
    pub fn req(
        &self,
        filters: Vec<Filter>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ReqMessage, RelayError>> + Send + 'static>> {
        // Clone everything the stream block needs to own.
        let socket = self.socket.clone();

        Box::pin(stream! {
            let sub_id = nanoid::nanoid!(12);
            let url = socket.url().to_string();

            let subscribe_msg = ClientMessage::Req {
                id: sub_id.clone(),
                filters: filters.clone(),
            }.to_json();
            let unsubscribe_msg = ClientMessage::Close { id: sub_id.clone() }.to_json();

            // Open a multiplexed channel filtered to our sub_id.
            // `_sender` is unused here — relay.rs uses socket.send() for
            // outbound. `_stream` holds the RAII guard; dropping it sends
            // CLOSE and starts the keepAlive timer.
            let sub_id_filter = sub_id.clone();
            let (mut channel, _sender) = match socket
                .multiplex(subscribe_msg, unsubscribe_msg, move |text| {
                    text.contains(&sub_id_filter)
                })
                .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    yield Err(RelayError::from(e));
                    return;
                }
            };

            // Synthetic Open — immediately confirms the REQ was sent.
            yield Ok(ReqMessage::Open {
                from: url.clone(),
                id: sub_id.clone(),
                filters: filters.clone(),
            });

            // Drive the multiplexed stream — it's already filtered to our sub_id.
            while let Some(text) = channel.next().await {
                match RelayMessage::from_json(&text) {
                    Ok(RelayMessage::Event { sub_id: sid, event }) if sid == sub_id => {
                        yield Ok(ReqMessage::Event { from: url.clone(), id: sid, event });
                    }
                    Ok(RelayMessage::Eose { sub_id: sid }) if sid == sub_id => {
                        yield Ok(ReqMessage::Eose { from: url.clone(), id: sid });
                    }
                    Ok(RelayMessage::Closed { sub_id: sid, reason }) if sid == sub_id => {
                        yield Ok(ReqMessage::Closed { from: url.clone(), id: sid, reason });
                        return; // relay closed — stream ends naturally
                    }
                    _ => {} // other message types (AUTH, NOTICE, etc.) — ignore
                }
            }

            // Channel closed (WS disconnected).
            yield Err(RelayError::ConnectionClosed);
            // channel drops here → DropGuard fires: CLOSE sent, keepAlive started
        })
    }

    /// Publish a Nostr event to the relay.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn publish(&self, event: NostrEvent) -> Result<PublishResponse, RelayError> {
        let event_id = event.id.clone();
        self.await_ok_response(ClientMessage::Event(event), event_id).await
    }

    /// Send a NIP-42 AUTH response event.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn auth(&self, event: NostrEvent) -> Result<PublishResponse, RelayError> {
        let event_id = event.id.clone();
        self.await_ok_response(ClientMessage::Auth(event), event_id).await
    }

    /// Send a NIP-45 COUNT request.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn count(&self, filters: Vec<Filter>) -> Result<CountResponse, RelayError> {
        let sub_id = nanoid::nanoid!(12);
        let url = self.socket.url().to_string();

        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;
        let (mut tx, rx) = oneshot::channel::<CountResponse>();
        let sub_id_task = sub_id.clone();
        let url_task = url.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    msg = rx_msg.recv() => match msg {
                        Ok(text) => {
                            if let Ok(RelayMessage::Count { sub_id: sid, count }) =
                                RelayMessage::from_json(&text)
                            {
                                if sid == sub_id_task {
                                    let _ = tx.send(CountResponse {
                                        from: url_task,
                                        sub_id: sid,
                                        count,
                                    });
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        _ => {}
                    }
                }
            }
        });

        self.socket
            .send(ClientMessage::Count { id: sub_id, filters }.to_json())
            .await
            .map_err(RelayError::from)?;

        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| RelayError::Timeout)?
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Expose the parsed relay message stream.
    ///
    /// **Connects automatically** if not already connected.
    pub async fn messages(
        &self,
    ) -> Result<std::pin::Pin<Box<dyn Stream<Item = RelayMessage> + Send + 'static>>, RelayError> {
        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;

        Ok(Box::pin(stream! {
            loop {
                match rx_msg.recv().await {
                    Ok(text) => match RelayMessage::from_json(&text) {
                        Ok(msg) => yield msg,
                        Err(e) => eprintln!("[rust-applesauce/relay] parse error: {e}"),
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[rust-applesauce/relay] messages() lagged by {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }))
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    pub fn url(&self) -> &str { self.socket.url() }

    pub fn is_connected(&self) -> bool { self.socket.is_connected() }

    pub fn subscription_count(&self) -> usize { self.socket.channel_count() }

    /// Access the underlying [`RelaySocket`] directly.
    pub fn socket(&self) -> &RelaySocket { &self.socket }

    // -----------------------------------------------------------------------
    // Convenience stream adapters
    // -----------------------------------------------------------------------

    /// Subscribe and return only the [`NostrEvent`]s — skipping `Open`,
    /// `Eose`, and `Closed` messages.
    ///
    /// This is shorthand for:
    /// ```ignore
    /// relay.req(filters).events()
    /// ```
    ///
    /// Connection errors still propagate as `Err(RelayError)`.  If you also
    /// need to react to `Eose` or `Closed`, use [`req`](Relay::req) directly.
    pub fn events(
        &self,
        filters: Vec<Filter>,
    ) -> Pin<Box<dyn Stream<Item = Result<NostrEvent, RelayError>> + Send + 'static>> {
        self.req(filters).events()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Spawn a listener for `RelayMessage::Ok` keyed on `event_id`, send
    /// `outbound`, and await the response with a 10-second timeout.
    ///
    /// Shared by [`publish`](Relay::publish) and [`auth`](Relay::auth) — the
    /// only difference between those two methods is which `ClientMessage`
    /// variant is sent.
    async fn await_ok_response(
        &self,
        outbound: ClientMessage,
        event_id: String,
    ) -> Result<PublishResponse, RelayError> {
        let url = self.socket.url().to_string();
        let mut rx_msg = self.socket.subscribe_raw().await.map_err(RelayError::from)?;
        let (mut tx, rx) = oneshot::channel::<PublishResponse>();
        let event_id_task = event_id.clone();
        let url_task = url.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Exit early if the caller dropped `rx` (e.g. on send failure).
                    _ = tx.closed() => break,
                    msg = rx_msg.recv() => match msg {
                        Ok(text) => {
                            if let Ok(RelayMessage::Ok { event_id: eid, ok, message }) =
                                RelayMessage::from_json(&text)
                            {
                                if eid == event_id_task {
                                    let _ = tx.send(PublishResponse {
                                        from: url_task,
                                        event_id: eid,
                                        ok,
                                        message,
                                    });
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        _ => {}
                    }
                }
            }
        });

        self.socket.send(outbound.to_json()).await.map_err(RelayError::from)?;

        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| RelayError::Timeout)?
            .map_err(|_| RelayError::ConnectionClosed)
    }
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

// ---------------------------------------------------------------------------
// ReqStreamExt — chainable adapters for req() streams
// ---------------------------------------------------------------------------

/// Extension trait that adds Nostr-specific adapters to any
/// `Stream<Item = Result<ReqMessage, RelayError>>` — i.e. the stream
/// returned by [`Relay::req`].
///
/// Import this trait to chain the adapters directly onto the stream:
///
/// ```no_run
/// # use rust_applesauce::{Filter, Relay};
/// # use rust_applesauce::ReqStreamExt;
/// # use futures_util::StreamExt;
/// # #[tokio::main] async fn main() {
/// let relay = Relay::new("wss://relay.damus.io");
/// let filter = Filter { kinds: Some(vec![1]), limit: Some(5), ..Default::default() };
///
/// // Only events — Open / Eose / Closed are silently dropped.
/// let mut stream = relay.req(vec![filter]).events();
/// while let Some(result) = stream.next().await {
///     let event = result.unwrap();
///     println!("{}", event.content);
/// }
/// # }
/// ```
pub trait ReqStreamExt: Stream<Item = Result<ReqMessage, RelayError>> + Sized + Send + 'static {
    /// Filter down to [`NostrEvent`]s only.
    ///
    /// `Open`, `Eose`, and `Closed` are silently discarded.
    /// Connection errors are preserved as `Err(RelayError)`.
    fn events(self) -> Pin<Box<dyn Stream<Item = Result<NostrEvent, RelayError>> + Send + 'static>> {
        Box::pin(self.filter_map(|item| async move {
            match item {
                Ok(ReqMessage::Event { event, .. }) => Some(Ok(event)),
                Ok(_) => None, // Open, Eose, Closed — discard
                Err(e) => Some(Err(e)),
            }
        }))
    }

    /// Filter down to [`NostrEvent`]s, stopping when `Eose` arrives.
    ///
    /// Yields only stored (pre-EOSE) events.  The stream ends naturally after
    /// `Eose` — useful for one-shot queries where you don't want the live feed.
    /// Connection errors are preserved as `Err(RelayError)`.
    fn until_eose(self) -> Pin<Box<dyn Stream<Item = Result<NostrEvent, RelayError>> + Send + 'static>> {
        Box::pin(stream! {
            let mut inner = Box::pin(self);
            while let Some(item) = inner.next().await {
                match item {
                    Ok(ReqMessage::Event { event, .. }) => yield Ok(event),
                    Ok(ReqMessage::Eose { .. }) => return, // stop cleanly
                    Ok(_) => {}                            // Open, Closed — skip
                    Err(e) => { yield Err(e); return; }
                }
            }
        })
    }
}

/// Blanket impl — automatically applies to the `Pin<Box<dyn Stream<...>>>`
/// returned by [`Relay::req`].
impl<S> ReqStreamExt for S
where
    S: Stream<Item = Result<ReqMessage, RelayError>> + Send + 'static,
{}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::time::Duration;

    /// Smoke test: Relay starts disconnected with zero subscriptions.
    #[test]
    fn relay_construction() {
        let relay = Relay::new("wss://relay.damus.io");
        assert_eq!(relay.url(), "wss://relay.damus.io");
        assert!(!relay.is_connected());
        assert_eq!(relay.subscription_count(), 0);
    }

    /// Relay with a custom keepAlive duration.
    #[test]
    fn relay_with_keep_alive() {
        let relay = Relay::with_keep_alive("wss://relay.damus.io", Duration::from_secs(5));
        assert_eq!(relay.url(), "wss://relay.damus.io");
        assert!(!relay.is_connected());
    }

    /// req() is now sync — calling it does NOT connect the socket.
    #[test]
    fn req_is_sync_and_does_not_connect() {
        let relay = Relay::new("wss://relay.damus.io");
        let _stream = relay.req(vec![crate::types::Filter::default()]);
        // Socket must still be closed — nothing was polled.
        assert!(!relay.is_connected());
        assert_eq!(relay.subscription_count(), 0);
    }

    /// Polling req() on a bad URL surfaces the error as the first stream item.
    #[tokio::test]
    async fn req_deferred_connect_fails_on_bad_url() {
        let relay = Relay::new("ws://127.0.0.1:1"); // port 1 — always refused
        let mut stream = relay.req(vec![crate::types::Filter::default()]);
        // First poll triggers the connect attempt.
        let first = stream.next().await;
        assert!(
            matches!(first, Some(Err(_))),
            "expected first item to be Err, got {first:?}"
        );
        // Sub count must not have been incremented on connect failure.
        assert_eq!(relay.subscription_count(), 0);
    }

    /// Dropping the stream sends CLOSE and decrements the sub count synchronously.
    /// After keepAlive the socket closes.
    #[tokio::test]
    async fn keep_alive_disconnects_after_last_sub() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = tokio_tungstenite::accept_async(stream).await;
                });
            }
        });

        let relay = Relay::with_keep_alive(
            format!("ws://{addr}"),
            Duration::from_millis(100),
        );

        let mut stream = relay.req(vec![crate::types::Filter::default()]);

        // First poll connects and emits Open — the socket is now live.
        let first = stream.next().await;
        assert!(
            matches!(first, Some(Ok(ReqMessage::Open { .. }))),
            "expected Open, got {first:?}"
        );
        assert!(relay.is_connected());
        assert_eq!(relay.subscription_count(), 1);

        // Drop the stream — RAII guard fires: CLOSE sent, sub_count → 0.
        drop(stream);

        assert_eq!(relay.subscription_count(), 0);

        // Wait for keepAlive to close the socket.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!relay.is_connected());
    }
}
