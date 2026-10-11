//! A minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`
//! (fd 3: commands in, fd 4: responses and events out, each message a JSON
//! object terminated by a NUL byte). The pipe means no DevTools port is
//! ever opened, so no other local process can drive the browser.
//!
//! Nothing here logs message parameters: some commands carry secrets
//! (`Input.insertText` while filling a saved login).

use crate::util::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// Commands normally answer in milliseconds; navigation waits are done
/// separately with their own deadline.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest single message accepted from the browser (screenshots of tall
/// pages are the biggest).
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// A protocol event (`method` + `params`), possibly from a target session.
#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>;

pub struct Cdp {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Pending>,
    next_id: AtomicU64,
    events: broadcast::Sender<Arc<Event>>,
    closed: watch::Receiver<bool>,
}

impl Cdp {
    /// Start the reader and writer tasks for a browser's pipe ends:
    /// `to_browser` is the write end of fd 3, `from_browser` the read end
    /// of fd 4.
    pub fn start(to_browser: OwnedFd, from_browser: OwnedFd) -> Result<Arc<Self>> {
        let mut writer = tokio::net::unix::pipe::Sender::from_owned_fd(to_browser)?;
        let mut reader = tokio::net::unix::pipe::Receiver::from_owned_fd(from_browser)?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (events, _) = broadcast::channel(256);
        let (closed_tx, closed) = watch::channel(false);
        let pending: Arc<Pending> = Arc::default();
        tokio::spawn(async move {
            while let Some(mut msg) = rx.recv().await {
                msg.push(0);
                if writer.write_all(&msg).await.is_err() {
                    break;
                }
            }
        });
        let (p, ev) = (pending.clone(), events.clone());
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut chunk = vec![0u8; 64 * 1024];
            'read: loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(end) = buf.iter().position(|b| *b == 0) {
                    let msg: Vec<u8> = buf.drain(..=end).collect();
                    dispatch(&msg[..msg.len() - 1], &p, &ev);
                }
                if buf.len() > MAX_MESSAGE {
                    crate::warn!("browser: oversized protocol message; closing");
                    break 'read;
                }
            }
            let _ = closed_tx.send(true);
            for (_, tx) in p.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                let _ = tx.send(Err("the browser closed".into()));
            }
        });
        Ok(Arc::new(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            events,
            closed,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Resolves when the browser's pipe closes.
    pub async fn wait_closed(&self) {
        let mut c = self.closed.clone();
        let _ = c.wait_for(|v| *v).await;
    }

    /// Send a command and wait for its result.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.call_timeout(method, params, session, CALL_TIMEOUT)
            .await
    }

    pub async fn call_timeout(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err("the browser is not running".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = s.into();
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if self.tx.send(serde_json::to_vec(&msg)?).is_err() {
            self.forget(id);
            return Err("the browser is not running".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(format!("{method}: {e}").into()),
            Ok(Err(_)) => Err("the browser closed".into()),
            Err(_) => {
                self.forget(id);
                Err(format!("{method}: no answer from the browser").into())
            }
        }
    }

    /// Fire-and-forget (screencast acknowledgements).
    pub fn send(&self, method: &str, params: Value, session: Option<&str>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = s.into();
        }
        if let Ok(bytes) = serde_json::to_vec(&msg) {
            let _ = self.tx.send(bytes);
        }
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

fn dispatch(raw: &[u8], pending: &Pending, events: &broadcast::Sender<Arc<Event>>) {
    let Ok(mut v) = serde_json::from_slice::<Value>(raw) else {
        return;
    };
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let Some(tx) = pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            return;
        };
        let res = match v.get_mut("error") {
            Some(e) => Err(e["message"]
                .as_str()
                .unwrap_or("protocol error")
                .to_string()),
            None => Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
        };
        let _ = tx.send(res);
        return;
    }
    let Some(method) = v.get("method").and_then(Value::as_str).map(String::from) else {
        return;
    };
    let _ = events.send(Arc::new(Event {
        method,
        params: v.get_mut("params").map(Value::take).unwrap_or(Value::Null),
        session_id: v.get("sessionId").and_then(Value::as_str).map(String::from),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_results_errors_and_events() {
        let pending: Pending = Mutex::default();
        let (ev, mut rx) = broadcast::channel(4);
        let (tx1, mut rx1) = oneshot::channel();
        let (tx2, mut rx2) = oneshot::channel();
        pending.lock().unwrap().insert(1, tx1);
        pending.lock().unwrap().insert(2, tx2);
        dispatch(br#"{"id":1,"result":{"ok":true}}"#, &pending, &ev);
        dispatch(
            br#"{"id":2,"error":{"code":-32000,"message":"No node"}}"#,
            &pending,
            &ev,
        );
        dispatch(
            br#"{"method":"Page.loadEventFired","params":{"timestamp":1},"sessionId":"S"}"#,
            &pending,
            &ev,
        );
        dispatch(b"not json", &pending, &ev);
        assert_eq!(rx1.try_recv().unwrap().unwrap()["ok"], true);
        assert_eq!(rx2.try_recv().unwrap().unwrap_err(), "No node");
        let e = rx.try_recv().unwrap();
        assert_eq!(e.method, "Page.loadEventFired");
        assert_eq!(e.session_id.as_deref(), Some("S"));
        assert!(pending.lock().unwrap().is_empty());
    }
}
