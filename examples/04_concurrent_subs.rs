//! # Example 04 — Two concurrent subscriptions over one socket
//!
//! Demonstrates the key multiplexing property: two independent `req()` calls
//! share a single WebSocket connection.  Each gets its own isolated stream —
//! events for subscription A never appear in subscription B's stream.
//!
//! Both streams are driven concurrently via `tokio::select!`.
//!
//! Run with:
//!   cargo run --example 04_concurrent_subs

use std::time::Duration;

use futures_util::StreamExt;
use rust_applesauce::{Filter, Relay, ReqMessage};

#[tokio::main]
async fn main() {
    // One relay handle — one WebSocket, shared by both subscriptions.
    let relay = Relay::new("wss://relay.damus.io");

    // Both streams are created synchronously before either connects.
    let mut stream_a = relay.req(vec![
        Filter { kinds: Some(vec![1]), limit: Some(8), ..Default::default() },
    ]);
    let mut stream_b = relay.req(vec![
        Filter { kinds: Some(vec![7]), limit: Some(8), ..Default::default() },
    ]);

    println!("Opening two subscriptions on {}...\n", relay.url());

    let mut eose_a = false;
    let mut eose_b = false;

    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!("\n[timeout] 15s elapsed");
                break;
            }

            item = stream_a.next() => {
                match item {
                    None => break,
                    Some(Err(e)) => { eprintln!("[A error] {e}"); break; }
                    Some(Ok(msg)) => {
                        handle_msg("A", msg, &mut eose_a);
                        if eose_a && eose_b {
                            println!("\nBoth subscriptions reached EOSE.");
                            break;
                        }
                    }
                }
            }

            item = stream_b.next() => {
                match item {
                    None => break,
                    Some(Err(e)) => { eprintln!("[B error] {e}"); break; }
                    Some(Ok(msg)) => {
                        handle_msg("B", msg, &mut eose_b);
                        if eose_a && eose_b {
                            println!("\nBoth subscriptions reached EOSE.");
                            break;
                        }
                    }
                }
            }
        }
    }

    // Dropping both streams sends CLOSE for each subscription.
    drop(stream_a);
    drop(stream_b);

    println!(
        "Subscription count after drop: {}",
        relay.subscription_count()
    );

    relay.disconnect();
    println!("Done.");
}

fn handle_msg(label: &str, msg: ReqMessage, eose: &mut bool) {
    match msg {
        ReqMessage::Open { id, .. } => {
            println!("[{label} open]    sub={id}");
        }
        ReqMessage::Event { event, .. } => {
            let kind_label = match event.kind {
                1 => "note    ",
                7 => "reaction",
                k => return eprintln!("[{label}] unexpected kind {k}"),
            };
            println!(
                "[{label} {kind_label}] {} | {}",
                &event.id[..8],
                truncate(&event.content, 60),
            );
        }
        ReqMessage::Eose { .. } => {
            println!("[{label} eose]");
            *eose = true;
        }
        ReqMessage::Closed { reason, .. } => {
            println!("[{label} closed] {reason}");
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{out}…") } else { out }
}
