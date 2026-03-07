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

use futures_util::StreamExt;
use rx_rs_nostr::{Filter, Relay, ReqMessage};

#[tokio::main]
async fn main() {
    let relay = Relay::new("wss://relay.damus.io");

    let filters = vec![
        Filter { kinds: Some(vec![1]), limit: Some(5), ..Default::default() },
        Filter { kinds: Some(vec![6]), limit: Some(5), ..Default::default() },
    ];

    // sync — nothing sent yet
    let mut stream = relay.req(filters);

    println!("Fetching kind-1 notes and kind-6 reposts from {}\n", relay.url());

    while let Some(result) = stream.next().await {
        match result {
            Err(e) => { eprintln!("[error] {e}"); break; }
            Ok(ReqMessage::Open { id, filters, .. }) => {
                println!("[open]  sub={id}  filters={}", filters.len());
            }
            Ok(ReqMessage::Event { event, .. }) => {
                let kind_label = match event.kind {
                    1 => "note  ",
                    6 => "repost",
                    k => { eprintln!("unexpected kind {k}"); continue; }
                };
                println!(
                    "[{}] {} | pubkey {}… | {}",
                    kind_label,
                    &event.id[..8],
                    &event.pubkey[..8],
                    truncate(&event.content, 64),
                );
            }
            Ok(ReqMessage::Eose { .. }) => {
                println!("\n[eose]  all stored events delivered — exiting");
                break;
            }
            Ok(ReqMessage::Closed { reason, .. }) => {
                println!("[closed] {reason}");
                break;
            }
        }
    }

    drop(stream);
    relay.disconnect();
    println!("Done.");
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{out}…") } else { out }
}
