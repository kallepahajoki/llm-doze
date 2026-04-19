use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::server::{ManagedServer, ServerState};

/// Start the backend service. Returns Ok(()) when health check passes.
pub async fn start_backend(server: &Arc<ManagedServer>) -> Result<(), String> {
    let name = &server.config.name;
    info!(server = %name, cmd = %server.config.start, "starting backend");

    if server.config.is_managed_subprocess() {
        start_managed_subprocess(server).await?;
    } else {
        start_fire_and_forget(server).await?;
    }

    // Poll health endpoint
    wait_for_health(server).await
}

async fn start_managed_subprocess(server: &Arc<ManagedServer>) -> Result<(), String> {
    let parts = shell_words(&server.config.start);
    if parts.is_empty() {
        return Err("Empty start command".to_string());
    }

    let child = Command::new(&parts[0])
        .args(&parts[1..])
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn '{}': {}", server.config.start, e))?;

    info!(
        server = %server.config.name,
        pid = child.id().unwrap_or(0),
        "spawned managed subprocess"
    );

    let mut guard = server.child_process.lock().await;
    *guard = Some(child);
    Ok(())
}

async fn start_fire_and_forget(server: &Arc<ManagedServer>) -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(&server.config.start)
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| format!("Failed to run start command: {}", e))?;

    if !status.success() {
        return Err(format!(
            "Start command exited with status {} for '{}'",
            status, server.config.name
        ));
    }
    Ok(())
}

async fn wait_for_health(server: &Arc<ManagedServer>) -> Result<(), String> {
    let url = server.config.backend_url(&server.config.health);
    let timeout = Duration::from_secs(server.config.startup_timeout);
    let poll_interval = Duration::from_secs(server.config.startup_poll_interval);
    let deadline = tokio::time::Instant::now() + timeout;

    info!(server = %server.config.name, url = %url, "polling health endpoint");

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Empty<bytes::Bytes>>();

    loop {
        match client
            .get(url.parse::<hyper::Uri>().unwrap())
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!(server = %server.config.name, "health check passed");
                return Ok(());
            }
            Ok(resp) => {
                tracing::debug!(
                    server = %server.config.name,
                    status = %resp.status(),
                    "health check returned non-2xx"
                );
            }
            Err(e) => {
                tracing::debug!(
                    server = %server.config.name,
                    error = %e,
                    "health check failed"
                );
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Health check timed out after {}s for '{}'",
                server.config.startup_timeout, server.config.name
            ));
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Stop the backend service.
pub async fn stop_backend(server: &Arc<ManagedServer>) -> Result<(), String> {
    let name = &server.config.name;
    info!(server = %name, "stopping backend");

    if server.config.is_managed_subprocess() {
        stop_managed_subprocess(server).await
    } else {
        stop_fire_and_forget(server).await
    }
}

async fn stop_managed_subprocess(server: &Arc<ManagedServer>) -> Result<(), String> {
    let mut guard = server.child_process.lock().await;
    if let Some(ref mut child) = *guard {
        let pid = child.id();
        info!(server = %server.config.name, pid = pid.unwrap_or(0), "killing managed subprocess");

        // Send SIGTERM first
        #[cfg(unix)]
        if let Some(pid) = pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            // Give it a grace period
            tokio::select! {
                _ = child.wait() => {},
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    warn!(server = %server.config.name, "subprocess did not exit after SIGTERM, sending SIGKILL");
                    let _ = child.kill().await;
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = child.kill().await;
        }

        let _ = child.wait().await;
        *guard = None;
        Ok(())
    } else {
        warn!(server = %server.config.name, "no child process to stop");
        Ok(())
    }
}

async fn stop_fire_and_forget(server: &Arc<ManagedServer>) -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(&server.config.stop)
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| format!("Failed to run stop command: {}", e))?;

    if !status.success() {
        warn!(
            server = %server.config.name,
            status = %status,
            "stop command exited with non-zero status"
        );
    }
    Ok(())
}

/// Background task that monitors idle time and stops the backend when idle.
pub async fn idle_monitor(server: Arc<ManagedServer>) {
    let check_interval = Duration::from_secs(10);

    loop {
        tokio::time::sleep(check_interval).await;

        let state = server.get_state().await;
        if state != ServerState::Running {
            continue;
        }

        let idle = server.idle_seconds().await;
        if idle >= server.config.idle_timeout {
            info!(
                server = %server.config.name,
                idle_seconds = idle,
                timeout = server.config.idle_timeout,
                "idle timeout reached, stopping backend"
            );

            server.set_state(ServerState::Stopping).await;
            match stop_backend(&server).await {
                Ok(()) => {
                    info!(server = %server.config.name, "backend stopped successfully");
                }
                Err(e) => {
                    error!(server = %server.config.name, error = %e, "failed to stop backend");
                }
            }
            server.set_state(ServerState::Stopped).await;
            server.stop_notify.notify_waiters();
        }
    }
}

/// Split a command string into parts, handling basic quoting.
fn shell_words(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for ch in cmd.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_words_simple() {
        assert_eq!(
            shell_words("/usr/bin/server -m model.gguf --port 8091"),
            vec!["/usr/bin/server", "-m", "model.gguf", "--port", "8091"]
        );
    }

    #[test]
    fn test_shell_words_quoted() {
        assert_eq!(
            shell_words(r#"/usr/bin/server -m "my model.gguf" --port 8091"#),
            vec!["/usr/bin/server", "-m", "my model.gguf", "--port", "8091"]
        );
    }

    #[test]
    fn test_shell_words_single_quotes() {
        assert_eq!(
            shell_words("/usr/bin/server -m 'my model.gguf'"),
            vec!["/usr/bin/server", "-m", "my model.gguf"]
        );
    }

    #[test]
    fn test_shell_words_empty() {
        assert_eq!(shell_words(""), Vec::<String>::new());
    }
}
