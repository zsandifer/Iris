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

    /// Register a live connection, counted as present; the guard
    /// unregisters on drop.
    pub fn connect(&self, user_id: &str) -> OnlineGuard {
        adjust(&self.online, user_id, true);
        OnlineGuard {
            online: self.online.clone(),
            user_id: user_id.to_string(),
            present: true,
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

/// Add (`up`) or remove one connection from a user's online count.
fn adjust(online: &Mutex<HashMap<String, usize>>, user_id: &str, up: bool) {
    let mut map = online.lock().unwrap_or_else(|p| p.into_inner());
    if up {
        *map.entry(user_id.to_string()).or_default() += 1;
    } else if let Some(n) = map.get_mut(user_id) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(user_id);
        }
    }
}

/// One connection's share of its user's online count. While the
/// connection is away (the app is in the background) it does not count,
/// so replies are pushed even though the socket is still open.
pub struct OnlineGuard {
    online: Arc<Mutex<HashMap<String, usize>>>,
    user_id: String,
    present: bool,
}

impl OnlineGuard {
    /// Mark this connection present or away. Repeating the current state
    /// changes nothing, so a double `away` never double-counts.
    pub fn set_present(&mut self, present: bool) {
        if self.present != present {
            self.present = present;
            adjust(&self.online, &self.user_id, present);
        }
    }
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        // An away connection was already uncounted.
        self.set_present(false);
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

    #[test]
    fn away_connections_do_not_count_as_online() {
        let hub = Hub::new();
        let mut phone = hub.connect("a");
        assert!(phone.present && hub.is_online("a"));
        phone.set_present(false);
        assert!(!hub.is_online("a"), "an away socket is not online");
        phone.set_present(false);
        assert!(!hub.is_online("a"));
        phone.set_present(true);
        phone.set_present(true);
        assert!(hub.is_online("a"));
        // A second socket keeps the user online while the first is away,
        // and a double away on one never uncounts the other.
        let desk = hub.connect("a");
        phone.set_present(false);
        phone.set_present(false);
        assert!(hub.is_online("a"));
        drop(desk);
        assert!(!hub.is_online("a"));
        // Dropping an away socket does not decrement again.
        let other = hub.connect("a");
        drop(phone);
        assert!(
            hub.is_online("a"),
            "the away socket's drop left the other alone"
        );
        drop(other);
        assert!(!hub.is_online("a"));
        assert!(hub.online.lock().unwrap().is_empty(), "no stale entries");
        // Users are counted separately.
        let _b = hub.connect("b");
        let mut a = hub.connect("a");
        a.set_present(false);
        assert!(hub.is_online("b") && !hub.is_online("a"));
        a.set_present(true);
        drop(a);
        assert!(!hub.is_online("a") && hub.is_online("b"));
    }
}
