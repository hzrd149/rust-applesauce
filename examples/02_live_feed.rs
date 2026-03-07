//! # Example 02 — Live feed after EOSE
//!
//! Subscribe to kind-1 notes.  Print stored events as they arrive, then
//! switch to printing live events as they come in.  Run for 30 seconds
//! then shut down cleanly.
//!
//! Run with:
//!   cargo run --example 02_live_feed

use std::time::Duration;

use rx_rs_nostr::{Filter, Relay, ReqMessage};
use rxrust::prelude::*;
use tokio::sync::mpsc;

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
        since: Some(now - 3600), // last hour of stored events
        ..Default::default()
    };

    let handle = relay
        .req(vec![filter])
        .await
        .expect("failed to open subscription");

    println!(
        "Streaming kind-1 notes from {} for 30 seconds...\n",
        relay.url()
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<ReqMessage>();

    let _sub = handle
        .subject
        .clone()
        .on_error(|e| eprintln!("[error] {e:?}"))
        .on_complete(|| println!("[stream complete]"))
        .subscribe(move |msg| {
            let _ = tx.send(msg);
        });

    // Phase tracking — are we in stored or live delivery?
    let mut live = false;
    let mut stored_count = 0usize;
    let mut live_count = 0usize;

    // Drive the loop for at most 30 seconds.
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            // Timeout branch — clean up and exit.
            _ = &mut deadline => {
                println!(
                    "\n[timeout] 30s elapsed.\
                     \n  stored events received : {stored_count}\
                     \n  live events received   : {live_count}"
                );
                break;
            }

            // Message branch.
            msg = rx.recv() => {
                let Some(msg) = msg else { break };

                match msg {
                    ReqMessage::Open { id, .. } => {
                        println!("[open]   sub id = {id}");
                    }
                    ReqMessage::Event { event, .. } => {
                        let preview = truncate(&event.content, 72);
                        if live {
                            live_count += 1;
                            println!(
                                "[live ]  {} | {}",
                                &event.id[..8], preview
                            );
                        } else {
                            stored_count += 1;
                            println!(
                                "[stored] {} | {}",
                                &event.id[..8], preview
                            );
                        }
                    }
                    ReqMessage::Eose { .. } => {
                        live = true;
                        println!(
                            "\n[eose]   {stored_count} stored events received — \
                             now streaming live...\n"
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

    drop(handle);
    relay.disconnect();
    println!("Done.");
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}
