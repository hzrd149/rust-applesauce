//! # Example 01 — Subscribing to events
//!
//! Three progressively simpler ways to get a stream of Nostr events.
//!
//! Run with:
//!   cargo run --example 01_subscribe

use futures_util::StreamExt;
use rust_applesauce::{Filter, Relay, ReqMessage, ReqStreamExt};

const RELAY: &str = "wss://relay.damus.io";

fn filter() -> Filter {
    Filter { kinds: Some(vec![1]), limit: Some(5), ..Default::default() }
}

// ---------------------------------------------------------------------------
// Style 1 — raw req(), full control
//
// You get every message type and handle all cases. Use this when you need
// to react to Eose, Closed, or errors differently.
// ---------------------------------------------------------------------------
async fn style_raw(relay: &Relay) {
    println!("\n--- Style 1: raw req() ---");

    let mut stream = relay.req(vec![filter()]);

    while let Some(result) = stream.next().await {
        match result {
            Ok(ReqMessage::Open { id, .. }) => println!("  [open] {id}"),
            Ok(ReqMessage::Event { event, .. }) => println!("  [event] {}", &event.id[..8]),
            Ok(ReqMessage::Eose { .. }) => { println!("  [eose]"); break; }
            Ok(ReqMessage::Closed { reason, .. }) => { println!("  [closed] {reason}"); break; }
            Err(e) => { eprintln!("  [error] {e}"); break; }
        }
    }
}

// ---------------------------------------------------------------------------
// Style 2 — .until_eose() adapter
//
// Yields only the stored events (before EOSE), then the stream ends.
// Items are Result<NostrEvent, RelayError> — no more match arms for Open /
// Eose / Closed.
// ---------------------------------------------------------------------------
async fn style_until_eose(relay: &Relay) {
    println!("\n--- Style 2: .until_eose() ---");

    let mut stream = relay.req(vec![filter()]).until_eose();

    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => println!("  [event] {}", &event.id[..8]),
            Err(e) => { eprintln!("  [error] {e}"); break; }
        }
    }
}

// ---------------------------------------------------------------------------
// Style 3 — relay.events() shorthand
//
// The simplest form. Returns only NostrEvents from the live feed.
// Equivalent to req(...).events() — discards Open / Eose / Closed silently.
// Use this when you just want the events and nothing else.
// ---------------------------------------------------------------------------
async fn style_events(relay: &Relay) {
    println!("\n--- Style 3: relay.events() ---");

    // Take the first 5 events then stop — using standard StreamExt::take().
    let mut stream = relay.events(vec![filter()]).take(5);

    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => println!("  [event] {} | {}", &event.id[..8], truncate(&event.content, 60)),
            Err(e) => { eprintln!("  [error] {e}"); break; }
        }
    }
}

#[tokio::main]
async fn main() {
    let relay = Relay::new(RELAY);
    println!("Connecting to {RELAY}");

    style_raw(&relay).await;
    style_until_eose(&relay).await;
    style_events(&relay).await;

    relay.disconnect();
    println!("\nDone.");
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{out}…") } else { out }
}
