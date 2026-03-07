//! # Example 04 — Two concurrent subscriptions over one socket
//!
//! Demonstrates the key multiplexing property: two independent `req()` calls
//! share a single WebSocket connection.  Each gets its own isolated stream of
//! messages — events for subscription A never appear in subscription B's stream.
//!
//! We run both subscriptions in parallel and print a labelled line for each
//! event so you can see the interleaving.
//!
//! Run with:
//!   cargo run --example 04_concurrent_subs

use std::time::Duration;

use rx_rs_nostr::{Filter, Relay, ReqMessage};
use rxrust::prelude::*;
use tokio::sync::mpsc;

// A tagged message so we know which subscription each event came from.
#[derive(Debug)]
struct Tagged {
    label: &'static str,
    msg: ReqMessage,
}

#[tokio::main]
async fn main() {
    // One relay handle — one WebSocket, shared by both subscriptions.
    let relay = Relay::new("wss://relay.damus.io");

    // -----------------------------------------------------------------------
    // Subscription A: recent kind-1 notes
    // -----------------------------------------------------------------------
    let filter_a = Filter {
        kinds: Some(vec![1]),
        limit: Some(8),
        ..Default::default()
    };

    // -----------------------------------------------------------------------
    // Subscription B: recent kind-7 reactions
    // -----------------------------------------------------------------------
    let filter_b = Filter {
        kinds: Some(vec![7]),
        limit: Some(8),
        ..Default::default()
    };

    // Open both subscriptions.  The first call connects the socket; the
    // second call reuses it immediately.
    let handle_a = relay.req(vec![filter_a]).await.expect("sub A failed");
    let handle_b = relay.req(vec![filter_b]).await.expect("sub B failed");

    println!(
        "Two subscriptions open on {}  (socket count = {})\n",
        relay.url(),
        relay.subscription_count(),
    );

    // -----------------------------------------------------------------------
    // Bridge both subjects into a single shared channel with a label tag.
    // -----------------------------------------------------------------------
    let (tx, mut rx) = mpsc::unbounded_channel::<Tagged>();

    let tx_a = tx.clone();
    let _sub_a = handle_a
        .subject
        .clone()
        .on_error(|e| eprintln!("[A error] {e:?}"))
        .on_complete(|| println!("[A complete]"))
        .subscribe(move |msg| {
            let _ = tx_a.send(Tagged { label: "A", msg });
        });

    let tx_b = tx.clone();
    let _sub_b = handle_b
        .subject
        .clone()
        .on_error(|e| eprintln!("[B error] {e:?}"))
        .on_complete(|| println!("[B complete]"))
        .subscribe(move |msg| {
            let _ = tx_b.send(Tagged { label: "B", msg });
        });

    // -----------------------------------------------------------------------
    // Drain until both subscriptions have sent EOSE (or 15 s timeout).
    // -----------------------------------------------------------------------
    let mut eose_a = false;
    let mut eose_b = false;

    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!("\n[timeout] 15 s elapsed");
                break;
            }
            tagged = rx.recv() => {
                let Some(Tagged { label, msg }) = tagged else { break };

                match msg {
                    ReqMessage::Open { id, .. } => {
                        println!("[{label} open]    sub={id}");
                    }
                    ReqMessage::Event { event, .. } => {
                        let kind_label = match event.kind {
                            1 => "note    ",
                            7 => "reaction",
                            k => return eprintln!("unexpected kind {k}"),
                        };
                        println!(
                            "[{label} {kind_label}] {} | {}",
                            &event.id[..8],
                            truncate(&event.content, 60),
                        );
                    }
                    ReqMessage::Eose { .. } => {
                        println!("[{label} eose]");
                        if label == "A" { eose_a = true; }
                        if label == "B" { eose_b = true; }
                        if eose_a && eose_b {
                            println!("\nBoth subscriptions reached EOSE — done.");
                            break;
                        }
                    }
                    ReqMessage::Closed { reason, .. } => {
                        println!("[{label} closed]   {reason}");
                        break;
                    }
                }
            }
        }
    }

    // Dropping both handles sends CLOSE for each subscription.
    // The socket stays open for keepAlive seconds, then closes.
    drop(handle_a);
    drop(handle_b);

    println!(
        "\nSubscription count after drop: {}",
        relay.subscription_count()
    );

    relay.disconnect();
    println!("Done.");
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{truncated}…") } else { truncated }
}
