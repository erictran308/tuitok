//! A client of the DevTools protocol (CDP), which Lightpanda and Chrome both
//! speak: JSON commands carrying an id, answers carrying the same id, and
//! events. Lightpanda serves it on a WebSocket (it has no pipe); Chrome over
//! `--remote-debugging-pipe`, each message ending in a NUL byte, so it opens
//! no port. One task writes and one reads; [`Cdp::call`] waits for its
//! answer, and events go to the receiver made with the client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::pipe;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::text;

/// The longest message taken: the feed's answers are a few hundred KB, and
/// a browser could otherwise make tuitok hold anything it sends.
pub const MAX_MESSAGE: usize = 64 << 20;

/// How long a command waits for its answer.
const CALL_WAIT: Duration = Duration::from_secs(15);

/// An event from the browser.
#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// The page's session it comes from, if any.
    pub session: Option<String>,
}

type Answer = std::result::Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Answer>>>>;

pub struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
}

impl Cdp {
    fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (out, outgoing) = mpsc::unbounded_channel();
        let cdp = Self {
            out,
            pending: Pending::default(),
            next: AtomicU64::new(1),
        };
        (cdp, outgoing)
    }

    /// A client over the WebSocket at `url`. The connection waits
    /// `CALL_WAIT` at most: a browser that takes it and never answers would
    /// otherwise hold the batch, and every batch after it, for good.
    pub async fn websocket(url: &str) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let connect = tokio_tungstenite::connect_async_with_config(url, Some(config), true);
        let (socket, _) = tokio::time::timeout(CALL_WAIT, connect)
            .await
            .context("no DevTools WebSocket: no answer")?
            .context("no DevTools WebSocket")?;
        let (mut sink, mut stream) = socket.split();
        let (cdp, mut outgoing) = Self::new();
        let (events, received) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(Message::text(message)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let pending = cdp.pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                match message {
                    Message::Text(text) => dispatch(text.as_str(), &pending, &events),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            lock(&pending).clear();
        });
        Ok((cdp, received))
    }

    /// A client over Chrome's pipe: `to` is Chrome's fd 3, `from` its fd 4.
    pub fn pipe(
        mut to: pipe::Sender,
        from: pipe::Receiver,
    ) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (cdp, mut outgoing) = Self::new();
        let (events, received) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let mut bytes = message.into_bytes();
                bytes.push(0);
                if to.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let pending = cdp.pending.clone();
        tokio::spawn(async move {
            let mut from = BufReader::new(from);
            let mut message = Vec::new();
            loop {
                let chunk = match from.fill_buf().await {
                    Ok([]) | Err(_) => break,
                    Ok(chunk) => chunk,
                };
                let (used, ended) = match chunk.iter().position(|&b| b == 0) {
                    Some(at) => {
                        message.extend_from_slice(&chunk[..at]);
                        (at + 1, true)
                    }
                    None => {
                        message.extend_from_slice(chunk);
                        (chunk.len(), false)
                    }
                };
                from.consume(used);
                if message.len() > MAX_MESSAGE {
                    break;
                }
                if ended {
                    if let Ok(text) = std::str::from_utf8(&message) {
                        dispatch(text, &pending, &events);
                    }
                    message.clear();
                }
            }
            lock(&pending).clear();
        });
        (cdp, received)
    }

    /// Sends `method` and waits for its answer: its `result`, or its error.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if self.out.send(message(id, method, params, session)).is_err() {
            lock(&self.pending).remove(&id);
            bail!("the browser closed");
        }
        match tokio::time::timeout(CALL_WAIT, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("{method}: {error}"),
            Ok(Err(_)) => bail!("the browser closed"),
            Err(_) => {
                lock(&self.pending).remove(&id);
                bail!("{method}: no answer in {} s", CALL_WAIT.as_secs())
            }
        }
    }

    /// Sends `method` without waiting: its answer is dropped.
    pub fn send(&self, method: &str, params: Value, session: Option<&str>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let _ = self.out.send(message(id, method, params, session));
    }
}

fn message(id: u64, method: &str, params: Value, session: Option<&str>) -> String {
    let mut message = json!({"id": id, "method": method, "params": params});
    if let Some(session) = session {
        message["sessionId"] = session.into();
    }
    message.to_string()
}

fn lock(pending: &Pending) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Answer>>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hands an answer to whoever waits for it, or an event to the receiver.
fn dispatch(text: &str, pending: &Pending, events: &mpsc::UnboundedSender<Event>) {
    let Ok(mut message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let Some(waiting) = lock(pending).remove(&id) else {
            return;
        };
        let error = message
            .get("error")
            .map(|error| text::one_line(error["message"].as_str().unwrap_or("failed"), 200));
        let answer = match error {
            Some(error) => Err(error),
            None => Ok(message
                .get_mut("result")
                .map(Value::take)
                .unwrap_or(Value::Null)),
        };
        let _ = waiting.send(answer);
        return;
    }
    let Some(method) = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let session = message
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = message
        .get_mut("params")
        .map(Value::take)
        .unwrap_or(Value::Null);
    let _ = events.send(Event {
        method,
        params,
        session,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_go_to_their_caller_and_events_to_the_receiver() {
        let pending = Pending::default();
        let (events, mut received) = mpsc::unbounded_channel();
        let (tx, mut rx) = oneshot::channel();
        lock(&pending).insert(7, tx);
        dispatch(r#"{"id":7,"result":{"ok":1}}"#, &pending, &events);
        assert_eq!(rx.try_recv().unwrap().unwrap(), json!({"ok": 1}));
        let (tx, mut rx) = oneshot::channel();
        lock(&pending).insert(8, tx);
        dispatch(
            r#"{"id":8,"error":{"message":"no\u001b[2J way"}}"#,
            &pending,
            &events,
        );
        assert_eq!(rx.try_recv().unwrap().unwrap_err(), "no[2J way");
        dispatch(
            r#"{"method":"Network.loadingFinished","params":{"requestId":"1"},"sessionId":"S"}"#,
            &pending,
            &events,
        );
        let event = received.try_recv().unwrap();
        assert_eq!(event.method, "Network.loadingFinished");
        assert_eq!(event.session.as_deref(), Some("S"));
        // Not JSON, or an answer nobody waits for: dropped.
        dispatch("nope", &pending, &events);
        dispatch(r#"{"id":99,"result":{}}"#, &pending, &events);
        assert!(received.try_recv().is_err());
    }
}
