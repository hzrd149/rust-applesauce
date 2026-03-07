//! # Example 03 — Multiple filters in one subscription
//!
//! A single REQ can carry multiple filters.  Here we ask for:
//!   - Filter A: the 5 most recent kind-1 notes (short text posts)
//!   - Filter B: the 5 most recent kind-6 reposts
//!
//! The relay merges the results into one stream.  We tag each event by
//! kind so the output clearly shows which filter matched.
//!
//! Run with:
//!   cargo run --example 03_multiple_filters

use rx_rs_nostr::{Filter, Relay, ReqMessage};
use rxrust::prelude::*;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let relay = Relay::new("wss://relay.damus.io");

    // Two filters in one subscription request.
    let filters = vec![
        // Short text notes
        Filter {
            kinds: Some(vec![1]),
            limit: Some(5),
            ..Default::default()
        },
        // Reposts
        Filter {
            kinds: Some(vec![6]),
            limit: Some(5),
            ..Default::default()
        },
    ];

    let handle = relay
        .req(filters)
        .await
        .expect("failed to open subscription");

    println!("Fetching kind-1 notes and kind-6 reposts from {}\n", relay.url());

    let (tx, mut rx) = mpsc::unbounded_channel::<ReqMessage>();

    let _sub = handle
        .subject
        .clone()
        .on_error(|e| eprintln!("[error] {e:?}"))
        .on_complete(|| println!("[stream complete]"))
        .subscribe(move |msg| {
            let _ = tx.send(msg);
        });

    while let Some(msg) = rx.recv().await {
        match msg {
            ReqMessage::Open { id, filters, .. } => {
                println!("[open]  sub={id}  filters={}", filters.len());
            }
            ReqMessage::Event { event, .. } => {
                let kind_label = match event.kind {
                    1 => "note  ",
                    6 => "repost",
                    k => return println!("[event] kind={k} (unexpected)"),
                };
                let preview = truncate(&event.content, 64);
                println!(
                    "[{}] {} | pubkey {}… | {}",
                    kind_label,
                    &event.id[..8],
                    &event.pubkey[..8],
                    preview,
                );
            }
            ReqMessage::Eose { .. } => {
                println!("\n[eose]  all stored events delivered — exiting");
                break;
            }
            ReqMessage::Closed { reason, .. } => {
                println!("[closed] {reason}");
                break;
            }
        }
    }

    drop(handle);
    relay.disconnect();
    println!("Done.");
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{truncated}…") } else { truncated }
}
