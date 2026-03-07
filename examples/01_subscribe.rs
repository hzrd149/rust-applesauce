//! # Example 01 — Basic subscription
//!
//! The simplest possible usage: connect to relay.damus.io, subscribe to the
//! 10 most recent kind-1 notes (short text posts), print each one, then exit.
//!
//! Run with:
//!   cargo run --example 01_subscribe

use rx_rs_nostr::{Filter, Relay, ReqMessage};
use rxrust::prelude::*;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    // ------------------------------------------------------------------
    // 1. Create the relay handle.
    //    No connection is made yet — it happens on the first req().
    // ------------------------------------------------------------------
    let relay = Relay::new("wss://relay.damus.io");

    // ------------------------------------------------------------------
    // 2. Build a filter: the 10 most recent kind-1 (text note) events.
    // ------------------------------------------------------------------
    let filter = Filter {
        kinds: Some(vec![1]),
        limit: Some(10),
        ..Default::default()
    };

    // ------------------------------------------------------------------
    // 3. Open the subscription.
    //    req() connects the WebSocket automatically and sends the REQ.
    // ------------------------------------------------------------------
    let handle = relay
        .req(vec![filter])
        .await
        .expect("failed to open subscription");

    println!("Connected to {}  (sub id printed below)", relay.url());

    // ------------------------------------------------------------------
    // 4. Subscribe to the stream.
    //
    //    We bridge into a Tokio channel so we can drive the loop from the
    //    main async context (rxRust subscribers run synchronously on the
    //    tokio task that pushes the value).
    // ------------------------------------------------------------------
    let (tx, mut rx) = mpsc::unbounded_channel::<ReqMessage>();

    let _sub = handle
        .subject
        .clone()
        .on_error(|e| eprintln!("[error] {e:?}"))
        .on_complete(|| println!("[stream complete]"))
        .subscribe(move |msg| {
            let _ = tx.send(msg);
        });

    // ------------------------------------------------------------------
    // 5. Process messages until EOSE, then stop.
    //    After EOSE the relay switches to live delivery — we exit here
    //    to keep the example short.  See 02_live_feed.rs for staying on.
    // ------------------------------------------------------------------
    while let Some(msg) = rx.recv().await {
        match msg {
            ReqMessage::Open { id, .. } => {
                println!("[open]   subscription id = {id}");
            }
            ReqMessage::Event { event, .. } => {
                let preview = event.content.chars().take(80).collect::<String>();
                let preview = if event.content.len() > 80 {
                    format!("{preview}…")
                } else {
                    preview
                };
                println!("[event]  {} | kind={} | {}", &event.id[..8], event.kind, preview);
            }
            ReqMessage::Eose { .. } => {
                println!("[eose]   end of stored events — exiting");
                break;
            }
            ReqMessage::Closed { reason, .. } => {
                println!("[closed] relay closed subscription: {reason}");
                break;
            }
        }
    }

    // ------------------------------------------------------------------
    // 6. Drop the handle → sends CLOSE to the relay, starts keepAlive.
    //    We disconnect explicitly here since we're done.
    // ------------------------------------------------------------------
    drop(handle);
    relay.disconnect();
    println!("Done.");
}
