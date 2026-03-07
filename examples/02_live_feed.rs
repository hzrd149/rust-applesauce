//! # Example 02 — Live feed after EOSE
//!
//! Subscribe to kind-1 notes.  Print stored events as they arrive, then
//! switch to printing live events as they come in.  Run for 30 seconds
//! then shut down cleanly.
//!
//! Run with:
//!   cargo run --example 02_live_feed

use std::time::Duration;

use futures_util::StreamExt;
use rx_rs_nostr::{Filter, Relay, ReqMessage};

#[tokio::main]
async fn main() {
    let relay = Relay::new("wss://relay.damus.io");

    // Kind-1 notes from the last hour.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let filter = Filter {
        kinds: Some(vec![1]),
        since: Some(now - 3600),
        ..Default::default()
    };

    // req() returns immediately — the socket opens on first .next().
    let mut stream = relay.req(vec![filter]);

    println!(
        "Streaming kind-1 notes from {} for 30 seconds...\n",
        relay.url()
    );

    let mut live = false;
    let mut stored_count = 0usize;
    let mut live_count = 0usize;

    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!(
                    "\n[timeout] 30s elapsed.\
                     \n  stored events: {stored_count}\
                     \n  live events  : {live_count}"
                );
                break;
            }
            item = stream.next() => {
                match item {
                    None => break,
                    Some(Err(e)) => { eprintln!("[error] {e}"); break; }
                    Some(Ok(msg)) => match msg {
                        ReqMessage::Open { id, .. } => {
                            println!("[open]   sub id = {id}");
                        }
                        ReqMessage::Event { event, .. } => {
                            let preview = truncate(&event.content, 72);
                            if live {
                                live_count += 1;
                                println!("[live ]  {} | {}", &event.id[..8], preview);
                            } else {
                                stored_count += 1;
                                println!("[stored] {} | {}", &event.id[..8], preview);
                            }
                        }
                        ReqMessage::Eose { .. } => {
                            live = true;
                            println!(
                                "\n[eose]   {stored_count} stored events — now streaming live...\n"
                            );
                        }
                        ReqMessage::Closed { reason, .. } => {
                            println!("[closed] {reason}");
                            break;
                        }
                    }
                }
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
