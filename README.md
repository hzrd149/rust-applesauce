# rust-applesauce

> **Experimental** — This is an attempt to port [`applesauce-relay`](https://github.com/hzrd149/applesauce/tree/master/packages/relay) from TypeScript/RxJS to Rust. It is not feature-complete and the API may change without notice.

A Nostr relay communication library for Rust, built on [tokio](https://tokio.rs/) and [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite). Modelled after the design of `applesauce-relay` — lazy connections, async streams, and a clean separation between the WebSocket layer and the Nostr protocol layer.

## Status

| Feature | Status |
|---|---|
| NIP-01 `REQ` / `EVENT` / `CLOSE` | done |
| Lazy WebSocket connection | done |
| keepAlive / auto-disconnect | done |
| `COUNT` (NIP-45) | done |
| `AUTH` (NIP-42) | done |
| Relay pool | not started |
| Relay groups | not started |
| Reconnection backoff | not started |
| Resubscribe on reconnect | not started |
| NIP-11 limitations | not started |
| Negentropy sync | not started |

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rust-applesauce = { git = "https://github.com/your-username/rust-applesauce" }
```

## Quick start

```rust
use rust_applesauce::{Filter, Relay, ReqMessage};
use futures_util::StreamExt;

#[tokio::main]
async fn main() {
    let relay = Relay::new("wss://relay.damus.io");

    let filter = Filter { kinds: Some(vec![1]), limit: Some(5), ..Default::default() };

    // req() is synchronous — nothing happens until the stream is polled.
    let mut stream = relay.req(vec![filter]);

    // First poll connects the socket, sends REQ, and starts emitting.
    while let Some(result) = stream.next().await {
        match result {
            Ok(ReqMessage::Event { event, .. }) => println!("{}", event.content),
            Ok(ReqMessage::Eose { .. })         => break,
            Ok(_)                               => {}
            Err(e)                              => { eprintln!("{e}"); break; }
        }
    }
    // Dropping the stream sends CLOSE; keepAlive timer starts.
}
```

### Stream adapters

```rust
use rust_applesauce::ReqStreamExt;

// Only yield stored events (before EOSE), then end the stream.
let events: Vec<_> = relay.req(vec![filter])
    .until_eose()
    .collect()
    .await;

// Skip all protocol messages and yield only NostrEvents.
let mut live = relay.events(vec![filter]);
while let Some(Ok(event)) = live.next().await {
    println!("{}", event.content);
}
```

## Examples

```
cargo run --example 01_subscribe
cargo run --example 02_live_feed
cargo run --example 03_multiple_filters
cargo run --example 04_concurrent_subs
```

## Design

The library has three layers:

- **`RelaySocket`** — manages the raw WebSocket connection, a shared broadcast channel, and the keepAlive timer. No Nostr knowledge.
- **`Relay`** — builds on `RelaySocket` to speak NIP-01: assigns subscription IDs, sends `REQ`/`CLOSE`/`EVENT`/`COUNT`/`AUTH`, and routes relay messages back to the correct stream.
- **`ReqStreamExt`** — stream adapters (`until_eose`, `events`) that layer ergonomic transformations on top of the raw `req()` stream.

`Relay` is cheaply `Clone`able — all clones share the same underlying socket.

## Upstream reference

TypeScript original: [`applesauce-relay`](https://github.com/hzrd149/applesauce/tree/master/packages/relay) by [@hzrd149](https://github.com/hzrd149).

## License

MIT
