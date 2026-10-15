//! Integration test: boot `serve` on an ephemeral port with a fake engine and
//! exercise every endpoint over real HTTP / WebSocket.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use sonder_core::{EngineSnapshot, HarmonySnapshot, LayerId, LayerSnapshot, Role, UserCommand};
use sonder_web::{serve_on, AudioSource, WebHandle};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

struct Sine {
    phase: f32,
    rate: f32,
}

impl AudioSource for Sine {
    fn pop_chunk(&mut self, out: &mut Vec<f32>) -> usize {
        // 10 ms of stereo 440 Hz per poll.
        let frames = (self.rate / 100.0) as usize;
        for _ in 0..frames {
            let s = (self.phase * std::f32::consts::TAU).sin() * 0.5;
            self.phase = (self.phase + 440.0 / self.rate).fract();
            out.push(s);
            out.push(s);
        }
        frames * 2
    }
}

fn fake_snapshot(t: f64) -> EngineSnapshot {
    let mut viz = BTreeMap::new();
    viz.insert(
        "traffic".to_string(),
        serde_json::json!({ "cars": [ { "x": 0.2, "y": 0.4, "lane": 1, "size": 0.5, "bright": 0.9 } ] }),
    );
    EngineSnapshot {
        scene_name: "Night Freeway".into(),
        viz_kind: "freeway".into(),
        time: t,
        harmony: HarmonySnapshot {
            tonic: 2,
            key_name: "D".into(),
            mode: "lydian".into(),
            scale: vec![2, 4, 6, 8, 9, 11, 1],
            chord: vec![2, 6, 9, 1],
            chord_name: "Dmaj7".into(),
            chord_degree: 1,
            beats_to_change: 4.0,
        },
        layers: vec![LayerSnapshot {
            id: LayerId(0),
            name: "traffic".into(),
            role: Role::Pad,
            instrument: "pad.glass".into(),
            source: Some("traffic".into()),
            ..Default::default()
        }],
        spectrum: vec![0.1; 48],
        viz,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end() {
    let (snap_tx, snap_rx) = watch::channel(Arc::new(fake_snapshot(0.0)));
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UserCommand>(16);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = WebHandle {
        snapshot: snap_rx,
        commands: cmd_tx,
        audio: Some(Box::new(Sine { phase: 0.0, rate: 48000.0 })),
        sample_rate: 48000,
    };
    let server = tokio::spawn(serve_on(listener, handle));

    // Keep the fake engine publishing at 20 Hz.
    let publisher = {
        let snap_tx = snap_tx.clone();
        tokio::spawn(async move {
            let mut t = 0.0;
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                t += 0.05;
                if snap_tx.send(Arc::new(fake_snapshot(t))).is_err() {
                    break;
                }
            }
        })
    };

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // --- static ---
    let index = client.get(&base).send().await.unwrap();
    assert!(index.status().is_success());
    let ct = index.headers()["content-type"].to_str().unwrap().to_string();
    assert!(ct.starts_with("text/html"));
    let html = index.text().await.unwrap();
    assert!(html.contains("/app.js") && html.contains("/app.css"));
    for (path, ct) in [("/app.js", "text/javascript"), ("/app.css", "text/css"), ("/worklet.js", "text/javascript")] {
        let r = client.get(format!("{base}{path}")).send().await.unwrap();
        assert!(r.status().is_success(), "{path}");
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with(ct), "{path}");
        assert!(!r.text().await.unwrap().is_empty(), "{path}");
    }

    // --- /api/snapshot ---
    let snap: serde_json::Value = client.get(format!("{base}/api/snapshot")).send().await.unwrap().json().await.unwrap();
    assert_eq!(snap["scene_name"], "Night Freeway");
    assert_eq!(snap["harmony"]["chord_name"], "Dmaj7");

    // --- /api/validate ---
    let v: serde_json::Value = client
        .post(format!("{base}/api/validate"))
        .json(&serde_json::json!({ "toml": "name = \"t\"\n[[layer]]\nname = \"a\"\nrole = \"pad\"\ninstrument = \"pad.warm\"\n" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["errors"], serde_json::json!([]));
    let v: serde_json::Value = client
        .post(format!("{base}/api/validate"))
        .json(&serde_json::json!({ "toml": "name = \"t\"\n[[layer]]\nname = \"a\"\n[[layer]]\nname = \"a\"\n" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!v["errors"].as_array().unwrap().is_empty(), "{v}");
    // The helper modules app.js imports are served too.
    for path in ["/scene-url.js", "/editor.js", "/geo.js", "/coastlines.json"] {
        let r = client.get(format!("{base}{path}")).send().await.unwrap();
        assert!(r.status().is_success(), "{path}");
    }

    // --- /ws: receive snapshot JSON, send a command ---
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
    let text = msg.into_text().unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["viz_kind"], "freeway");
    assert!(v["viz"]["traffic"]["cars"].is_array());

    // A second message must arrive from the 20 Hz publisher, and time advances.
    let msg2 = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
    let v2: serde_json::Value = serde_json::from_str(&msg2.into_text().unwrap()).unwrap();
    assert!(v2["time"].as_f64().unwrap() > 0.0);

    ws.send(Message::text(r#"{"cmd":"toggle_mute","layer":0}"#)).await.unwrap();
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv()).await.unwrap().unwrap();
    assert_eq!(cmd, UserCommand::ToggleMute { layer: LayerId(0) });

    ws.send(Message::text(r#"{"cmd":"nudge_tempo","delta":2.0}"#)).await.unwrap();
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv()).await.unwrap().unwrap();
    assert_eq!(cmd, UserCommand::NudgeTempo { delta: 2.0 });

    // Garbage is ignored, not fatal.
    ws.send(Message::text("not json")).await.unwrap();
    ws.send(Message::text(r#"{"cmd":"toggle_pause"}"#)).await.unwrap();
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv()).await.unwrap().unwrap();
    assert_eq!(cmd, UserCommand::TogglePause);
    ws.close(None).await.unwrap();

    // --- /audio: header then binary PCM frames of 20 ms ---
    let (mut aws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/audio")).await.unwrap();
    let header = tokio::time::timeout(Duration::from_secs(2), aws.next()).await.unwrap().unwrap().unwrap();
    assert!(header.is_text());
    let h: serde_json::Value = serde_json::from_str(&header.into_text().unwrap()).unwrap();
    assert_eq!(h["sample_rate"], 48000);
    assert_eq!(h["channels"], 2);
    assert_eq!(h["format"], "s16le");

    let mut frames = 0;
    while frames < 3 {
        let m = tokio::time::timeout(Duration::from_secs(3), aws.next()).await.unwrap().unwrap().unwrap();
        if let Message::Binary(b) = m {
            // 960 frames × 2 channels × 2 bytes.
            assert_eq!(b.len(), 960 * 2 * 2);
            // A sine at 0.5 amplitude: at least one sample clearly non-zero.
            let nonzero = b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).filter(|s| s.abs() > 1000).count();
            assert!(nonzero > 0);
            frames += 1;
        }
    }
    aws.close(None).await.unwrap();

    // --- graceful shutdown when the engine ends ---
    publisher.abort();
    drop(snap_tx);
    let result = tokio::time::timeout(Duration::from_secs(5), server).await.expect("server should stop").unwrap();
    result.unwrap();
}

#[tokio::test]
async fn audio_endpoint_without_tap_reports_error() {
    let (_snap_tx, snap_rx) = watch::channel(Arc::new(EngineSnapshot::default()));
    let (cmd_tx, _cmd_rx) = mpsc::channel::<UserCommand>(16);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = WebHandle { snapshot: snap_rx, commands: cmd_tx, audio: None, sample_rate: 48000 };
    let _server = tokio::spawn(serve_on(listener, handle));

    let (mut aws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/audio")).await.unwrap();
    let m = tokio::time::timeout(Duration::from_secs(2), aws.next()).await.unwrap().unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(&m.into_text().unwrap()).unwrap();
    assert_eq!(v["error"], "no audio tap");
    // Server closes next.
    let next = tokio::time::timeout(Duration::from_secs(2), aws.next()).await.unwrap();
    assert!(matches!(next, Some(Ok(Message::Close(_))) | None));
}

/// Graceful shutdown must not wait on a browser that keeps `/audio` open: the
/// audio broadcast never closes by itself, so the handler has to watch the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_completes_with_audio_client_connected() {
    let (snap_tx, snap_rx) = watch::channel(Arc::new(fake_snapshot(0.0)));
    let (cmd_tx, _cmd_rx) = mpsc::channel::<UserCommand>(16);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = WebHandle {
        snapshot: snap_rx,
        commands: cmd_tx,
        audio: Some(Box::new(Sine { phase: 0.0, rate: 48000.0 })),
        sample_rate: 48000,
    };
    let server = tokio::spawn(serve_on(listener, handle));

    let (mut aws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/audio")).await.unwrap();
    let header = tokio::time::timeout(Duration::from_secs(2), aws.next()).await.unwrap().unwrap().unwrap();
    assert!(header.is_text());
    // Keep the client connected and reading while the engine goes away.
    let reader = tokio::spawn(async move {
        let mut closed = false;
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(5), aws.next()).await {
            if matches!(m, Ok(Message::Close(_)) | Err(_)) {
                closed = true;
                break;
            }
        }
        closed
    });
    drop(snap_tx);
    let result = tokio::time::timeout(Duration::from_secs(5), server).await.expect("server should stop").unwrap();
    result.unwrap();
    assert!(reader.await.unwrap(), "audio client should see a Close frame");
}
