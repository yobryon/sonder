//! `/ws`: snapshot push + command receive.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use sonder_core::UserCommand;

use crate::AppState;

pub(crate) async fn snapshot_ws(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle(socket, state))
}

async fn handle(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.snapshot.clone();

    // Send the current state right away so the page paints before the next tick.
    let first = serde_json::to_string(&**rx.borrow_and_update());
    match first {
        Ok(json) => {
            if socket.send(Message::text(json)).await.is_err() {
                return;
            }
        }
        Err(e) => tracing::warn!(error = %e, "snapshot serialization failed"),
    }

    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    // Engine ended.
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                // `borrow_and_update` always yields the latest value, so a slow
                // client naturally coalesces intermediate snapshots.
                let json = match serde_json::to_string(&**rx.borrow_and_update()) {
                    Ok(j) => j,
                    Err(e) => { tracing::warn!(error = %e, "snapshot serialization failed"); continue; }
                };
                if socket.send(Message::text(json)).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<UserCommand>(&text) {
                            Ok(cmd) => {
                                tracing::debug!(?cmd, "web command");
                                if state.commands.send(cmd).await.is_err() {
                                    tracing::warn!("command channel closed");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, text = %text, "bad command from web client"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {} // binary / ping / pong: ignore (pings are auto-answered)
                    Some(Err(e)) => { tracing::debug!(error = %e, "ws error"); return; }
                }
            }
        }
    }
}
