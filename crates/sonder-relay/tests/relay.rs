//! A fake host and two guests against the real router over TCP.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn serve(ttl: Duration) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, sonder_relay::router(sonder_relay::Relay::with_ttl(ttl))).await.unwrap();
    });
    format!("127.0.0.1:{}", addr.port())
}

async fn ws(url: &str) -> Ws {
    let (s, _) = tokio_tungstenite::connect_async(url).await.expect("connect");
    s
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

/// Next text frame as JSON (3 s timeout).
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(3), ws.next()).await.expect("timed out waiting for a frame").expect("stream ended").unwrap();
        match m {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Close(_) => panic!("closed"),
            _ => continue,
        }
    }
}

/// Frames until one of `ty` arrives.
async fn recv_type(ws: &mut Ws, ty: &str) -> Value {
    for _ in 0..50 {
        let v = recv(ws).await;
        if v["type"] == ty {
            return v;
        }
    }
    panic!("no `{ty}` frame");
}

async fn recv_nothing(ws: &mut Ws, for_ms: u64) {
    if let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_millis(for_ms), ws.next()).await {
        panic!("unexpected frame: {t}");
    }
}

fn hello(name: &str) -> Value {
    json!({ "type": "hello", "scene_toml": "name = \"Night Freeway\"\n", "seed": 7, "epoch_unix": 1.7e9, "name": name, "public": true })
}

#[tokio::test]
async fn hello_replay_presence_chat_and_host_replacement() {
    let addr = serve(Duration::from_secs(60)).await;
    let http = reqwest::Client::new();

    // No room yet.
    let r = http.get(format!("http://{addr}/r/test")).send().await.unwrap();
    assert_eq!(r.status(), 404);

    let mut host = ws(&format!("ws://{addr}/r/test/host")).await;
    assert_eq!(recv_type(&mut host, "presence").await["listeners"], 0);
    send(&mut host, hello("bryon")).await;
    send(&mut host, json!({ "type": "snapshot", "time": 12.5, "captions": [], "form": {} })).await;

    // A late guest gets the hello first, then presence.
    let mut g1 = ws(&format!("ws://{addr}/r/test/join")).await;
    let h = recv(&mut g1).await;
    assert_eq!(h["type"], "hello");
    assert_eq!(h["seed"], 7);
    assert_eq!(recv_type(&mut g1, "presence").await["listeners"], 1);
    assert_eq!(recv_type(&mut host, "presence").await["listeners"], 1);

    let mut g2 = ws(&format!("ws://{addr}/r/test/join")).await;
    assert_eq!(recv(&mut g2).await["type"], "hello");
    assert_eq!(recv_type(&mut g2, "presence").await["listeners"], 2);
    assert_eq!(recv_type(&mut g1, "presence").await["listeners"], 2);
    assert_eq!(recv_type(&mut host, "presence").await["listeners"], 2);

    // Room info and the index.
    let info: sonder_relay::RoomInfo = http.get(format!("http://{addr}/r/test")).send().await.unwrap().json().await.unwrap();
    assert!(info.present);
    assert_eq!(info.listeners, 2);
    assert_eq!(info.title, "Night Freeway");
    assert_eq!(info.name, "bryon");
    assert!(info.host_time >= 12.5);
    let index = http.get(format!("http://{addr}/")).send().await.unwrap().text().await.unwrap();
    assert!(index.contains("test") && index.contains("Night Freeway") && index.contains("2 listening"), "{index}");

    // Host data fans out to both guests, not back to the host.
    send(&mut host, json!({ "type": "samples", "items": [{ "t": 13.0, "s": "traffic", "d": { "type": "reading", "values": { "count": 3.0 } } }] })).await;
    assert_eq!(recv_type(&mut g1, "samples").await["items"][0]["s"], "traffic");
    assert_eq!(recv_type(&mut g2, "samples").await["items"][0]["s"], "traffic");

    // Chat from a guest reaches everyone, including the host; chat from the host reaches guests.
    send(&mut g1, json!({ "type": "chat", "from": "g1", "text": "hello room" })).await;
    let c = recv_type(&mut g2, "chat").await;
    assert_eq!(c["from"], "g1");
    assert_eq!(c["text"], "hello room");
    assert!(c["at"].as_f64().unwrap() >= 12.5, "{c}");
    assert_eq!(recv_type(&mut g1, "chat").await["text"], "hello room");
    assert_eq!(recv_type(&mut host, "chat").await["text"], "hello room");
    send(&mut host, json!({ "type": "chat", "at": 14.0, "from": "bryon", "text": "welcome" })).await;
    assert_eq!(recv_type(&mut g1, "chat").await["from"], "bryon");
    assert_eq!(recv_type(&mut g2, "chat").await["at"], 14.0);
    recv_nothing(&mut host, 150).await;

    // Rate limit: a second line within a second is dropped (the sender gets an error).
    send(&mut g1, json!({ "type": "chat", "from": "g1", "text": "too fast" })).await;
    assert_eq!(recv_type(&mut g1, "error").await["type"], "error");
    recv_nothing(&mut g2, 200).await;

    // Ping → pong with the host's extrapolated engine time.
    send(&mut g2, json!({ "type": "ping", "sent": 123 })).await;
    let p = recv_type(&mut g2, "pong").await;
    assert_eq!(p["sent"], 123);
    assert!(p["host_time"].as_f64().unwrap() >= 12.5);
    assert!(p["relay_ms"].as_f64().unwrap() > 1.6e12);

    // A second host replaces the first; guests get the new hello.
    let mut host2 = ws(&format!("ws://{addr}/r/test/host")).await;
    assert_eq!(recv_type(&mut host2, "presence").await["listeners"], 2);
    let e = recv_type(&mut host, "error").await;
    assert!(e["error"].as_str().unwrap().contains("replaced"));
    send(&mut host2, hello("someone else")).await;
    assert_eq!(recv_type(&mut g1, "hello").await["name"], "someone else");
    let info: sonder_relay::RoomInfo = http.get(format!("http://{addr}/r/test")).send().await.unwrap().json().await.unwrap();
    assert!(info.present);
    assert_eq!(info.name, "someone else");
    assert_eq!(info.listeners, 2);

    // A guest leaving updates presence.
    g2.close(None).await.unwrap();
    assert_eq!(recv_type(&mut g1, "presence").await["listeners"], 1);
    assert_eq!(recv_type(&mut host2, "presence").await["listeners"], 1);
}

#[tokio::test]
async fn room_expires_after_the_host_leaves() {
    let addr = serve(Duration::from_millis(300)).await;
    let http = reqwest::Client::new();
    let mut host = ws(&format!("ws://{addr}/r/gone/host")).await;
    send(&mut host, hello("h")).await;
    let mut g = ws(&format!("ws://{addr}/r/gone/join")).await;
    assert_eq!(recv(&mut g).await["type"], "hello");
    host.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let info: sonder_relay::RoomInfo = http.get(format!("http://{addr}/r/gone")).send().await.unwrap().json().await.unwrap();
    assert!(!info.present);
    // The guest is told, then the room is gone.
    assert_eq!(recv_type(&mut g, "closed").await["type"], "closed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(http.get(format!("http://{addr}/r/gone")).send().await.unwrap().status(), 404);
    // A host coming back within the TTL keeps the room (and its guests).
    let addr = serve(Duration::from_secs(5)).await;
    let mut host = ws(&format!("ws://{addr}/r/back/host")).await;
    send(&mut host, hello("h")).await;
    let mut g = ws(&format!("ws://{addr}/r/back/join")).await;
    assert_eq!(recv(&mut g).await["type"], "hello");
    host.close(None).await.unwrap();
    let mut host = ws(&format!("ws://{addr}/r/back/host")).await;
    send(&mut host, hello("h again")).await;
    assert_eq!(recv_type(&mut g, "hello").await["name"], "h again");
}

#[tokio::test]
async fn bad_room_ids_are_rejected() {
    let addr = serve(Duration::from_secs(5)).await;
    assert!(tokio_tungstenite::connect_async(format!("ws://{addr}/r/{}/join", "x".repeat(70))).await.is_err());
}
