use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub token: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub listen: u16,
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

impl ServerConfig {
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
        if self.servers.is_empty() {
            return Err("No servers configured".into());
        }

        let mut ports = std::collections::HashSet::new();
        for server in &self.servers {
            if !ports.insert(server.listen) {
                return Err(format!(
                    "Duplicate listen port {} for server '{}'",
                    server.listen, server.name
                )
                .into());
            }
            if server.name.is_empty() {
                return Err("Server name cannot be empty".into());
            }
            if server.backend.is_empty() {
                return Err(format!(
                    "Backend address cannot be empty for server '{}'",
                    server.name
                )
                .into());
            }
        }
        Ok(())
    }

    pub fn effective_token<'a>(&'a self, server: &'a ServerConfig) -> Option<&'a str> {
        // Per-server auth takes priority
        if let Some(ref server_auth) = server.auth {
            if server_auth.enabled {
                return Some(&server_auth.token);
            } else {
                return None;
            }
        }
        // Fall back to global auth
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

servers:
  - name: test-server
    listen: 8000
    backend: localhost:8900
    start: echo start
    stop: echo stop
    health: /health
    idle_timeout: 600
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "test-server");
        assert_eq!(config.servers[0].listen, 8000);
        assert_eq!(config.auth.as_ref().unwrap().token, "test-token");
    }

    #[test]
    fn test_parse_minimal_config() {
        let yaml = r#"
servers:
  - name: minimal
    listen: 9000
    backend: localhost:9001
    start: echo start
    stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let server = &config.servers[0];
        assert_eq!(server.health, "/health");
        assert_eq!(server.idle_timeout, 600);
        assert_eq!(server.startup_timeout, 300);
        assert_eq!(server.startup_poll_interval, 2);
        assert!(config.auth.is_none());
    }

    #[test]
    fn test_managed_subprocess_detection() {
        let yaml = r#"
servers:
  - name: managed
    listen: 8000
    backend: localhost:8001
    start: /usr/bin/server
    stop: managed-subprocess
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.servers[0].is_managed_subprocess());
    }

    #[test]
    fn test_per_server_auth_override() {
        let yaml = r#"
auth:
  token: "global-token"

servers:
  - name: with-override
    listen: 8000
    backend: localhost:8001
    start: echo start
    stop: echo stop
    auth:
      token: "server-token"
  - name: uses-global
    listen: 8001
    backend: localhost:8002
    start: echo start
    stop: echo stop
  - name: no-auth
    listen: 8002
    backend: localhost:8003
    start: echo start
    stop: echo stop
    auth:
      token: ""
      enabled: false
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.effective_token(&config.servers[0]),
            Some("server-token")
        );
        assert_eq!(
            config.effective_token(&config.servers[1]),
            Some("global-token")
        );
        assert_eq!(config.effective_token(&config.servers[2]), None);
    }

    #[test]
    fn test_validate_duplicate_ports() {
        let yaml = r#"
servers:
  - name: server1
    listen: 8000
    backend: localhost:8001
    start: echo start
    stop: echo stop
  - name: server2
    listen: 8000
    backend: localhost:8002
    start: echo start
    stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_servers() {
        let yaml = r#"
servers: []
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_backend_url() {
        let yaml = r#"
servers:
  - name: test
    listen: 8000
    backend: localhost:8001
    start: echo start
    stop: echo stop
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.servers[0].backend_url("/v1/chat"),
            "http://localhost:8001/v1/chat"
        );
    }
}
