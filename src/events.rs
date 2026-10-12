//! In-process event hub: every server event is broadcast once and each
//! WebSocket connection forwards only the events addressed to its user.
//! Also tracks which users currently have an app window connected, so
//! push notifications can be limited to users who are away.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum Audience {
    All,
    Users(Vec<String>),
    /// The owner's devices only (for example Iris's computer).
    Owner,
}

impl Audience {
    #[cfg(test)]
    pub fn includes(&self, user_id: &str) -> bool {
        self.reaches(user_id, false)
    }

    /// Whether a connection of this user (who may be the owner) gets it.
    pub fn reaches(&self, user_id: &str, is_owner: bool) -> bool {
        match self {
            Audience::All => true,
            Audience::Users(u) => u.iter().any(|x| x == user_id),
            Audience::Owner => is_owner,
        }
    }
}

#[derive(Debug)]
pub struct Envelope {
    pub audience: Audience,
    /// Serialized once, sent to many sockets.
    pub json: String,
}

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Arc<Envelope>>,
    online: Arc<Mutex<HashMap<String, usize>>>,
    devices: Arc<Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            tx,
            online: Arc::default(),
            devices: Arc::default(),
        }
    }

    /// All sockets of a device share a durable cancellation signal; it cannot
    /// be lost when the event broadcast buffer overflows.
    pub fn watch_device(&self, device_id: &str) -> tokio::sync::watch::Receiver<bool> {
        let mut devices = self.devices.lock().unwrap_or_else(|p| p.into_inner());
        devices.retain(|_, tx| tx.receiver_count() > 0);
        devices
            .entry(device_id.into())
            .or_insert_with(|| tokio::sync::watch::channel(false).0)
            .subscribe()
    }

    pub fn revoke_device(&self, device_id: &str) {
        if let Some(tx) = self
            .devices
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(device_id)
        {
            tx.send_replace(true);
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Envelope>> {
        self.tx.subscribe()
    }

    /// Broadcast `event` (an object with a `type` field) to `audience`.
    pub fn send(&self, audience: Audience, mut event: Value) {
        if let Some(obj) = event.as_object_mut() {
            obj.insert("at".into(), crate::util::now_ms().into());
        }
        let _ = self.tx.send(Arc::new(Envelope {
            audience,
            json: event.to_string(),
        }));
    }

    pub fn to_users(&self, users: &[String], event: Value) {
        self.send(Audience::Users(users.to_vec()), event);
    }

    /// Register a live connection; the guard unregisters on drop.
    pub fn connect(&self, user_id: &str) -> OnlineGuard {
        *self
            .online
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(user_id.to_string())
            .or_default() += 1;
        OnlineGuard {
            online: self.online.clone(),
            user_id: user_id.to_string(),
        }
    }

    pub fn is_online(&self, user_id: &str) -> bool {
        self.online
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(user_id)
            .is_some_and(|n| *n > 0)
    }
}

pub struct OnlineGuard {
    online: Arc<Mutex<HashMap<String, usize>>>,
    user_id: String,
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        let mut map = self.online.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = map.get_mut(&self.user_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.user_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn revocation_cancels_all_device_sockets_only() {
        let hub = Hub::new();
        let mut a = hub.watch_device("a");
        let mut b = hub.watch_device("a");
        let other = hub.watch_device("b");
        hub.revoke_device("a");
        a.changed().await.unwrap();
        b.changed().await.unwrap();
        assert!(*a.borrow() && *b.borrow());
        assert!(!*other.borrow());
        assert!(*hub.watch_device("a").borrow());
    }

    #[tokio::test]
    async fn audience_and_presence() {
        let hub = Hub::new();
        let mut rx = hub.subscribe();
        hub.to_users(&["a".into()], json!({"type": "x"}));
        let env = rx.recv().await.unwrap();
        assert!(env.audience.reaches("a", false) && !env.audience.reaches("b", true));
        assert!(Audience::Owner.reaches("o", true) && !Audience::Owner.reaches("o", false));
        let v: Value = serde_json::from_str(&env.json).unwrap();
        assert_eq!(v["type"], "x");
        assert!(v["at"].is_i64());
        assert!(!hub.is_online("a"));
        let g1 = hub.connect("a");
        let g2 = hub.connect("a");
        drop(g1);
        assert!(hub.is_online("a"));
        drop(g2);
        assert!(!hub.is_online("a"));
    }
}
