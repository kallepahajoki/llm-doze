use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    pub listeners: Vec<ListenerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub token: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenerConfig {
    pub port: u16,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Only one route can be active at a time. Requesting a different model
    /// will stop the current backend before starting the new one.
    #[serde(default)]
    pub exclusive: bool,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    pub backend: String,
    pub start: String,
    pub stop: String,
    #[serde(default = "default_health")]
    pub health: String,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout: u64,
    #[serde(default = "default_startup_poll_interval")]
    pub startup_poll_interval: u64,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

fn default_true() -> bool {
    true
}

fn default_health() -> String {
    "/health".to_string()
}

fn default_idle_timeout() -> u64 {
    600
}

fn default_startup_timeout() -> u64 {
    300
}

fn default_startup_poll_interval() -> u64 {
    2
}

impl RouteConfig {
    pub fn is_managed_subprocess(&self) -> bool {
        self.stop == "managed-subprocess"
    }

    pub fn backend_url(&self, path: &str) -> String {
        format!("http://{}{}", self.backend, path)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.listeners.is_empty() {
            return Err("No listeners configured".into());
        }

        let mut ports = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();

        for listener in &self.listeners {
            if !ports.insert(listener.port) {
                return Err(format!("Duplicate listener port {}", listener.port).into());
            }

            if listener.routes.is_empty() {
                return Err(format!(
                    "Listener on port {} has no routes",
                    listener.port
                )
                .into());
            }

            // If multiple routes, all must have model names
            if listener.routes.len() > 1 {
                let mut models = std::collections::HashSet::new();
                for route in &listener.routes {
                    let model = route.model.as_ref().ok_or_else(|| {
                        format!(
                            "Route '{}' on port {} must have a 'model' field (multiple routes share this port)",
                            route.name, listener.port
                        )
                    })?;
                    if !models.insert(model) {
                        return Err(format!(
                            "Duplicate model '{}' on port {}",
                            model, listener.port
                        )
                        .into());
                    }
                }
            }

            for route in &listener.routes {
                if route.name.is_empty() {
                    return Err("Route name cannot be empty".into());
                }
                if !names.insert(&route.name) {
                    return Err(format!("Duplicate route name '{}'", route.name).into());
                }
                if route.backend.is_empty() {
                    return Err(format!(
                        "Backend address cannot be empty for route '{}'",
                        route.name
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Resolve auth token: route > listener > global. Returns None if auth is disabled.
    pub fn effective_token<'a>(
        &'a self,
        listener: &'a ListenerConfig,
        route: &'a RouteConfig,
    ) -> Option<&'a str> {
        // Per-route auth takes priority
        if let Some(ref route_auth) = route.auth {
            if route_auth.enabled {
                return Some(&route_auth.token);
            } else {
                return None;
            }
        }
        // Per-listener auth
        if let Some(ref listener_auth) = listener.auth {
            if listener_auth.enabled {
                return Some(&listener_auth.token);
            } else {
                return None;
            }
        }
        // Global auth
        if let Some(ref global_auth) = self.auth {
            if global_auth.enabled {
                return Some(&global_auth.token);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_config() {
        let yaml = r#"
auth:
  token: "test-token"

listeners:
  - port: 8000
    routes:
      - name: test-server
        backend: localhost:8900
        start: echo start
        stop: echo stop
        health: /health
        idle_timeout: 600
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.listeners.len(), 1);
        assert_eq!(config.listeners[0].routes[0].name, "test-server");
        assert_eq!(config.listeners[0].port, 8000);
        assert_eq!(config.auth.as_ref().unwrap().token, "test-token");
    }

    #[test]
    fn test_parse_minimal_config() {
        let yaml = r#"
listeners:
  - port: 9000
    routes:
      - name: minimal
        backend: localhost:9001
        start: echo start
        stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let route = &config.listeners[0].routes[0];
        assert_eq!(route.health, "/health");
        assert_eq!(route.idle_timeout, 600);
        assert_eq!(route.startup_timeout, 300);
        assert_eq!(route.startup_poll_interval, 2);
        assert!(config.auth.is_none());
    }

    #[test]
    fn test_managed_subprocess_detection() {
        let yaml = r#"
listeners:
  - port: 8000
    routes:
      - name: managed
        backend: localhost:8001
        start: /usr/bin/server
        stop: managed-subprocess
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.listeners[0].routes[0].is_managed_subprocess());
    }

    #[test]
    fn test_multi_route_requires_model() {
        let yaml = r#"
listeners:
  - port: 8000
    routes:
      - name: route1
        backend: localhost:8001
        start: echo start
        stop: echo stop
      - name: route2
        backend: localhost:8002
        start: echo start
        stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_multi_route_with_models() {
        let yaml = r#"
listeners:
  - port: 8000
    routes:
      - name: route1
        model: model-a
        backend: localhost:8001
        start: echo start
        stop: echo stop
      - name: route2
        model: model-b
        backend: localhost:8002
        start: echo start
        stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_auth_resolution_three_tiers() {
        let yaml = r#"
auth:
  token: "global-token"

listeners:
  - port: 8000
    auth:
      token: "listener-token"
    routes:
      - name: with-route-auth
        backend: localhost:8001
        start: echo start
        stop: echo stop
        auth:
          token: "route-token"
      - name: uses-listener
        model: model-b
        backend: localhost:8002
        start: echo start
        stop: echo stop
  - port: 9000
    routes:
      - name: uses-global
        backend: localhost:9001
        start: echo start
        stop: echo stop
  - port: 9001
    routes:
      - name: no-auth
        backend: localhost:9002
        start: echo start
        stop: echo stop
        auth:
          token: ""
          enabled: false
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let l0 = &config.listeners[0];
        let l1 = &config.listeners[1];
        let l2 = &config.listeners[2];

        assert_eq!(config.effective_token(l0, &l0.routes[0]), Some("route-token"));
        assert_eq!(config.effective_token(l0, &l0.routes[1]), Some("listener-token"));
        assert_eq!(config.effective_token(l1, &l1.routes[0]), Some("global-token"));
        assert_eq!(config.effective_token(l2, &l2.routes[0]), None);
    }

    #[test]
    fn test_validate_duplicate_ports() {
        let yaml = r#"
listeners:
  - port: 8000
    routes:
      - name: server1
        backend: localhost:8001
        start: echo start
        stop: echo stop
  - port: 8000
    routes:
      - name: server2
        backend: localhost:8002
        start: echo start
        stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_listeners() {
        let yaml = r#"
listeners: []
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_backend_url() {
        let yaml = r#"
listeners:
  - port: 8000
    routes:
      - name: test
        backend: localhost:8001
        start: echo start
        stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.listeners[0].routes[0].backend_url("/v1/chat"),
            "http://localhost:8001/v1/chat"
        );
    }

    #[test]
    fn test_validate_duplicate_names() {
        let yaml = r#"
listeners:
  - port: 8000
    routes:
      - name: same-name
        backend: localhost:8001
        start: echo start
        stop: echo stop
  - port: 9000
    routes:
      - name: same-name
        backend: localhost:9001
        start: echo start
        stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }
}
