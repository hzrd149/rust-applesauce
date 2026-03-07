//! # rx-rs-nostr
//!
//! A stateless Nostr relay connection library built on top of rxRust and
//! tokio-tungstenite.
//!
//! ## Quick start
//!
//! ```no_run
//! use rx_rs_nostr::{Relay, Filter};
//! use rxrust::prelude::*;
//!
//! #[tokio::main]
//! async fn main() {
//!     // No explicit connect() needed — req() connects automatically.
//!     let relay = Relay::new("wss://relay.damus.io");
//!
//!     let filter = Filter {
//!         kinds: Some(vec![1]),
//!         limit: Some(10),
//!         ..Default::default()
//!     };
//!
    //!     // req() is async and connects the WebSocket if not already open.
    //!     // It returns a ReqHandle. Deref it to access the SharedSubject.
    //!     let handle = relay.req(vec![filter]).await.unwrap();
    //!
    //!     // Chain .on_error() and .on_complete() for full lifecycle handling.
    //!     let _sub = handle.subject
    //!         .clone()
    //!         .on_error(|e| eprintln!("error: {e:?}"))
    //!         .on_complete(|| println!("done"))
    //!         .subscribe(|msg| println!("{msg:?}"));
    //!
    //!     // When handle is dropped, a 30-second keepAlive timer starts.
    //!     // If no new subscriptions arrive in that window, the socket closes.
//! }
//! ```

pub mod relay;
pub mod socket;
pub mod types;

pub use relay::{Relay, ReqHandle, DEFAULT_KEEP_ALIVE};
pub use socket::RelaySocket;
pub use types::{
    ClientMessage, CountResponse, Filter, NostrEvent, PublishResponse, ReqMessage, RelayError,
    RelayMessage,
};

#[cfg(test)]
mod tests {
    use super::*;
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
        // Should start with ["REQ","test123",{...}]
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

    /// Verify that req() on a bad URL returns an error (no panic, no hang).
    /// This also validates the lazy-connect path without needing a real relay.
    #[tokio::test]
    async fn req_lazy_connect_fails_on_bad_url() {
        let relay = Relay::new("ws://127.0.0.1:1"); // port 1 — always refused
        let result = relay.req(vec![Filter::default()]).await;
        assert!(result.is_err(), "expected connection error, got Ok");
        // Sub count must not have been incremented on connect failure.
        assert_eq!(relay.subscription_count(), 0);
    }

    /// Verify keepAlive fires and disconnects after the last sub drops.
    #[tokio::test]
    async fn keep_alive_disconnects_after_last_sub() {
        use tokio::net::TcpListener;

        // Spin up a minimal echo TCP server that accepts but does nothing —
        // just enough for the WS handshake to complete.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accept connections and perform a bare WebSocket handshake.
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = tokio_tungstenite::accept_async(stream).await;
                    // Just let the connection sit open.
                });
            }
        });

        // Use a very short keepAlive so the test completes quickly.
        let relay = Relay::with_keep_alive(
            format!("ws://{addr}"),
            Duration::from_millis(100),
        );

        // req() should connect automatically.
        let handle = relay.req(vec![Filter::default()]).await.unwrap();
        assert!(relay.is_connected(), "should be connected after req()");
        assert_eq!(relay.subscription_count(), 1);

        // Drop the ReqHandle — RAII guard sends CLOSE and decrements sub_count.
        drop(handle);

        // sub_count drops synchronously on drop() — no need to yield.
        assert_eq!(relay.subscription_count(), 0, "sub count should be 0 after drop");

        // Wait for keepAlive to fire and close the connection.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!relay.is_connected(), "connection should be closed after keepAlive");
    }
}
