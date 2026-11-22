//! Test helpers shared by the integration tests.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type Subs = Arc<Mutex<Vec<(mpsc::UnboundedSender<String>, String, Value)>>>;

fn matches(filter: &Value, ev: &Value) -> bool {
    let kind_ok = filter["kinds"]
        .as_array()
        .is_none_or(|k| k.contains(&ev["kind"]));
    let p_ok = filter["#p"].as_array().is_none_or(|want| {
        ev["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|t| t[0] == "p" && want.contains(&t[1])))
    });
    kind_ok && p_ok
}

/// A minimal NIP-01 relay: REQ, CLOSE, and EVENT answered with OK and fanned
/// out to every matching subscription. `refuse` turns every EVENT down.
pub async fn relay(refuse: bool) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}", listener.local_addr()?);
    let subs: Subs = Arc::default();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let subs = subs.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let (mut sink, mut stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                tokio::spawn(async move {
                    while let Some(m) = rx.recv().await {
                        if sink.send(Message::text(m)).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(Message::Text(t))) = stream.next().await {
                    let Ok(Value::Array(msg)) = serde_json::from_str::<Value>(&t) else {
                        continue;
                    };
                    match msg.first().and_then(Value::as_str) {
                        Some("REQ") => {
                            let id = msg[1].as_str().unwrap_or("").to_owned();
                            if let Ok(mut s) = subs.lock() {
                                s.push((tx.clone(), id, msg[2].clone()));
                            }
                        }
                        Some("EVENT") => {
                            let ev = &msg[1];
                            if refuse {
                                let _ = tx.send(
                                    json!(["OK", ev["id"], false, "blocked: this relay is closed"])
                                        .to_string(),
                                );
                                continue;
                            }
                            let _ = tx.send(json!(["OK", ev["id"], true, ""]).to_string());
                            if let Ok(s) = subs.lock() {
                                for (to, id, f) in s.iter() {
                                    if matches(f, ev) {
                                        let _ = to.send(json!(["EVENT", id, ev]).to_string());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    Ok(url)
}
