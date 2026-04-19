use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::{Mutex, Notify, RwLock};
use std::time::Instant;

use crate::config::ServerConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl std::fmt::Display for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerState::Stopped => write!(f, "stopped"),
            ServerState::Starting => write!(f, "starting"),
            ServerState::Running => write!(f, "running"),
            ServerState::Stopping => write!(f, "stopping"),
        }
    }
}

pub struct ManagedServer {
    pub config: ServerConfig,
    pub state: RwLock<ServerState>,
    pub last_request: RwLock<Instant>,
    pub startup_notify: Notify,
    pub stop_notify: Notify,
    pub child_process: Mutex<Option<Child>>,
    pub auth_token: Option<String>,
}

impl ManagedServer {
    pub fn new(config: ServerConfig, auth_token: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: RwLock::new(ServerState::Stopped),
            last_request: RwLock::new(Instant::now()),
            startup_notify: Notify::new(),
            stop_notify: Notify::new(),
            child_process: Mutex::new(None),
            auth_token,
        })
    }

    pub async fn get_state(&self) -> ServerState {
        *self.state.read().await
    }

    pub async fn set_state(&self, new_state: ServerState) {
        let mut state = self.state.write().await;
        tracing::info!(
            server = %self.config.name,
            from = %*state,
            to = %new_state,
            "state transition"
        );
        *state = new_state;
    }

    pub async fn touch(&self) {
        let mut last = self.last_request.write().await;
        *last = Instant::now();
    }

    pub async fn idle_seconds(&self) -> u64 {
        let last = self.last_request.read().await;
        last.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ServerConfig {
        ServerConfig {
            name: "test".to_string(),
            listen: 8000,
            backend: "localhost:8001".to_string(),
            start: "echo start".to_string(),
            stop: "echo stop".to_string(),
            health: "/health".to_string(),
            idle_timeout: 60,
            startup_timeout: 30,
            startup_poll_interval: 1,
            auth: None,
        }
    }

    #[tokio::test]
    async fn test_initial_state_is_stopped() {
        let server = ManagedServer::new(test_config(), None);
        assert_eq!(server.get_state().await, ServerState::Stopped);
    }

    #[tokio::test]
    async fn test_state_transitions() {
        let server = ManagedServer::new(test_config(), None);
        server.set_state(ServerState::Starting).await;
        assert_eq!(server.get_state().await, ServerState::Starting);
        server.set_state(ServerState::Running).await;
        assert_eq!(server.get_state().await, ServerState::Running);
        server.set_state(ServerState::Stopping).await;
        assert_eq!(server.get_state().await, ServerState::Stopping);
        server.set_state(ServerState::Stopped).await;
        assert_eq!(server.get_state().await, ServerState::Stopped);
    }

    #[tokio::test]
    async fn test_touch_updates_last_request() {
        let server = ManagedServer::new(test_config(), None);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let before = server.idle_seconds().await;
        server.touch().await;
        let after = server.idle_seconds().await;
        assert!(after <= before);
    }

    #[tokio::test]
    async fn test_auth_token_stored() {
        let server = ManagedServer::new(test_config(), Some("secret".to_string()));
        assert_eq!(server.auth_token.as_deref(), Some("secret"));
    }
}
