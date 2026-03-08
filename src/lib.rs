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

