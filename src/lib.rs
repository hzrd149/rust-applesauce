//! # rust-applesauce
//!
//! A stateless Nostr relay connection library built on top of async Rust
//! streams and tokio-tungstenite.
//!
//! ## Quick start
//!
//! ```no_run
//! use rust_applesauce::{Filter, Relay, ReqMessage};
//! use futures_util::StreamExt; // .next()
//!
//! #[tokio::main]
//! async fn main() {
//!     let relay = Relay::new("wss://relay.damus.io");
//!
//!     let filter = Filter { kinds: Some(vec![1]), limit: Some(5), ..Default::default() };
//!
//!     // req() is synchronous — nothing happens until the stream is polled.
//!     let mut stream = relay.req(vec![filter]);
//!
//!     // First poll connects the socket, sends REQ, and starts emitting.
//!     while let Some(result) = stream.next().await {
//!         match result {
//!             Ok(ReqMessage::Event { event, .. }) => println!("{}", event.content),
//!             Ok(ReqMessage::Eose { .. })         => break, // stored events done
//!             Ok(_)                               => {}
//!             Err(e)                              => { eprintln!("{e}"); break; }
//!         }
//!     }
//!     // Dropping stream sends CLOSE; keepAlive timer starts.
//! }
//! ```

pub mod relay;
pub mod socket;
pub mod types;

pub use relay::{Relay, ReqStreamExt, DEFAULT_KEEP_ALIVE};
pub use socket::RelaySocket;
pub use types::{
    ClientMessage, CountResponse, Filter, NostrEvent, PublishResponse, ReqMessage, RelayError,
    RelayMessage,
};

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

    /// Smoke test: Filter defaults are all None / empty.
    #[test]
    fn filter_default() {
        let f = Filter::default();
        assert!(f.ids.is_none());
        assert!(f.authors.is_none());
        assert!(f.kinds.is_none());
        assert!(f.since.is_none());
        assert!(f.until.is_none());
        assert!(f.limit.is_none());
    }

    /// Smoke test: ClientMessage serialization produces valid NIP-01 JSON.
    #[test]
    fn client_message_req_serialization() {
        let msg = ClientMessage::Req {
            id: "test123".into(),
            filters: vec![Filter {
                kinds: Some(vec![1]),
                limit: Some(5),
                ..Default::default()
            }],
        };
        let json = msg.to_json();
        assert!(json.starts_with(r#"["REQ","test123","#));
    }

    #[test]
    fn relay_message_parse_event() {
        let json = r#"["EVENT","sub1",{"id":"abc","pubkey":"def","created_at":1000,"kind":1,"tags":[],"content":"hello","sig":"xyz"}]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Event { sub_id, event } => {
                assert_eq!(sub_id, "sub1");
                assert_eq!(event.content, "hello");
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn relay_message_parse_eose() {
        let json = r#"["EOSE","sub1"]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Eose { sub_id } => assert_eq!(sub_id, "sub1"),
            _ => panic!("expected Eose"),
        }
    }

    #[test]
    fn relay_message_parse_ok() {
        let json = r#"["OK","eventid123",true,""]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Ok { event_id, ok, message } => {
                assert_eq!(event_id, "eventid123");
                assert!(ok);
                assert_eq!(message, "");
            }
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn relay_message_parse_notice() {
        let json = r#"["NOTICE","hello world"]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Notice { message } => assert_eq!(message, "hello world"),
            _ => panic!("expected Notice"),
        }
    }

    /// req() is now sync — calling it does NOT connect the socket.
    #[test]
    fn req_is_sync_and_does_not_connect() {
        let relay = Relay::new("wss://relay.damus.io");
        let _stream = relay.req(vec![Filter::default()]);
        // Socket must still be closed — nothing was polled.
        assert!(!relay.is_connected());
        assert_eq!(relay.subscription_count(), 0);
    }

    /// Polling req() on a bad URL surfaces the error as the first stream item.
    #[tokio::test]
    async fn req_deferred_connect_fails_on_bad_url() {
        let relay = Relay::new("ws://127.0.0.1:1"); // port 1 — always refused
        let mut stream = relay.req(vec![Filter::default()]);
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

        let mut stream = relay.req(vec![Filter::default()]);

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
