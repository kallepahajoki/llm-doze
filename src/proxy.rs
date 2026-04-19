use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tracing::{error, info, warn};

use crate::auth;
use crate::lifecycle;
use crate::server::{ManagedServer, ServerState};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub async fn handle_request(
    server: Arc<ManagedServer>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    // Auth check
    if let Err(resp) = auth::check_auth(&req, server.auth_token.as_deref()) {
        return Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()));
    }

    // Ensure backend is running
    if let Err(msg) = ensure_running(&server).await {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &msg));
    }

    // Update last request time
    server.touch().await;

    // Forward request
    forward_request(&server, req).await
}

async fn ensure_running(server: &Arc<ManagedServer>) -> Result<(), String> {
    loop {
        let state = server.get_state().await;
        match state {
            ServerState::Running => return Ok(()),
            ServerState::Stopped => {
                // We'll try to start it — transition to Starting
                server.set_state(ServerState::Starting).await;
                match lifecycle::start_backend(server).await {
                    Ok(()) => {
                        server.set_state(ServerState::Running).await;
                        server.startup_notify.notify_waiters();
                        return Ok(());
                    }
                    Err(e) => {
                        error!(server = %server.config.name, error = %e, "failed to start backend");
                        server.set_state(ServerState::Stopped).await;
                        server.startup_notify.notify_waiters();
                        return Err(e);
                    }
                }
            }
            ServerState::Starting => {
                // Another request is already starting it — wait
                info!(server = %server.config.name, "waiting for backend to finish starting");
                server.startup_notify.notified().await;
                // Loop to re-check state
            }
            ServerState::Stopping => {
                // Wait for stop to complete, then we'll restart
                warn!(server = %server.config.name, "backend is stopping, waiting before restart");
                server.stop_notify.notified().await;
                // Loop to re-check state (should be Stopped now)
            }
        }
    }
}

async fn forward_request(
    server: &Arc<ManagedServer>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    let client = Client::builder(TokioExecutor::new()).build_http::<Incoming>();

    let path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let uri = format!("http://{}{}", server.config.backend, path);

    // Build forwarded request
    let mut builder = Request::builder().method(req.method()).uri(&uri);

    // Copy headers, skip host
    for (key, value) in req.headers() {
        if key != "host" {
            builder = builder.header(key, value);
        }
    }

    let forwarded_req = match builder.body(req.into_body()) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to build forwarded request");
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build request",
            ));
        }
    };

    match client.request(forwarded_req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            let boxed_body = body.map_err(|e| -> BoxError { Box::new(e) }).boxed();
            Ok(Response::from_parts(parts, boxed_body))
        }
        Err(e) => {
            error!(
                server = %server.config.name,
                backend = %server.config.backend,
                error = %e,
                "failed to forward request to backend"
            );
            Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Backend error: {}", e),
            ))
        }
    }
}

fn error_response(status: StatusCode, message: &str) -> Response<BoxBody<Bytes, BoxError>> {
    let body = format!("{{\"error\": \"{}\"}}", message);
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|e| -> BoxError { Box::new(e) })
                .boxed(),
        )
        .unwrap()
}
