use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::auth;
use crate::lifecycle;
use crate::server::{ManagedServer, ServerState};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub enum ListenerRouter {
    /// Single backend, no model routing
    Single(Arc<ManagedServer>),
    /// Multiple backends, route by model field in request body
    Multi {
        routes: HashMap<String, Arc<ManagedServer>>,
    },
    /// Multiple backends but only one can run at a time.
    /// If the active backend matches, stream directly. If not, stop the
    /// current one and start the requested one.
    Exclusive {
        routes: HashMap<String, Arc<ManagedServer>>,
        active: RwLock<Option<String>>, // model name of the currently active route
    },
}

pub async fn handle_request(
    router: Arc<ListenerRouter>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    match &*router {
        ListenerRouter::Single(server) => handle_single(Arc::clone(server), req).await,
        ListenerRouter::Multi { routes } => handle_multi(routes, req).await,
        ListenerRouter::Exclusive { routes, active } => {
            handle_exclusive(routes, active, req).await
        }
    }
}

/// Single-route: stream body directly (zero buffering)
async fn handle_single(
    server: Arc<ManagedServer>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    if let Err(resp) = auth::check_auth(&req, server.auth_token.as_deref()) {
        return Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()));
    }

    if let Err(msg) = ensure_running(&server).await {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &msg));
    }

    server.touch().await;
    forward_request(&server, req).await
}

/// Multi-route: buffer body, extract model, route to matching backend
async fn handle_multi(
    routes: &HashMap<String, Arc<ManagedServer>>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    if !has_body(req.method()) {
        return handle_bodyless_multi(routes, &req).await;
    }

    let first_server = routes.values().next().unwrap();
    if let Err(resp) = auth::check_auth(&req, first_server.auth_token.as_deref()) {
        return Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()));
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(error = %e, "failed to read request body");
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Failed to read request body",
            ));
        }
    };

    let model_name = match extract_model(&body_bytes) {
        Some(name) => name,
        None => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Missing or invalid 'model' field in request body",
            ));
        }
    };

    let server = match routes.get(&model_name) {
        Some(s) => Arc::clone(s),
        None => {
            return Ok(unknown_model_response(&model_name, routes));
        }
    };

    if let Err(msg) = ensure_running(&server).await {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &msg));
    }

    server.touch().await;
    let rebuilt = Request::from_parts(parts, Full::new(body_bytes));
    forward_request(&server, rebuilt).await
}

/// Exclusive-route: only one backend runs at a time.
/// If the active backend matches the requested model, stream directly.
/// If a different model is requested, stop the current backend first.
async fn handle_exclusive(
    routes: &HashMap<String, Arc<ManagedServer>>,
    active: &RwLock<Option<String>>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    if !has_body(req.method()) {
        // For GET requests: if there's an active backend, forward to it
        let current = active.read().await.clone();
        if let Some(ref model) = current {
            if let Some(server) = routes.get(model) {
                if let Err(resp) = auth::check_auth(&req, server.auth_token.as_deref()) {
                    return Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()));
                }
                server.touch().await;
                return forward_request(server, req).await;
            }
        }
        return handle_bodyless_multi(routes, &req).await;
    }

    let first_server = routes.values().next().unwrap();
    if let Err(resp) = auth::check_auth(&req, first_server.auth_token.as_deref()) {
        return Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()));
    }

    // Buffer body to extract model
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(error = %e, "failed to read request body");
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Failed to read request body",
            ));
        }
    };

    let requested_model = match extract_model(&body_bytes) {
        Some(name) => name,
        None => {
            // No model in body — if there's an active backend, use it
            let current = active.read().await.clone();
            if let Some(ref model) = current {
                if let Some(server) = routes.get(model) {
                    if server.get_state().await == ServerState::Running {
                        server.touch().await;
                        let rebuilt = Request::from_parts(parts, Full::new(body_bytes));
                        return forward_request(server, rebuilt).await;
                    }
                }
            }
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Missing 'model' field and no active backend",
            ));
        }
    };

    let server = match routes.get(&requested_model) {
        Some(s) => Arc::clone(s),
        None => {
            return Ok(unknown_model_response(&requested_model, routes));
        }
    };

    // Check if we need to switch models
    let current = active.read().await.clone();
    if current.as_deref() != Some(&requested_model) {
        // Need to stop the current backend first
        if let Some(ref current_model) = current {
            if let Some(current_server) = routes.get(current_model) {
                let state = current_server.get_state().await;
                if state == ServerState::Running || state == ServerState::Starting {
                    info!(
                        from = %current_model,
                        to = %requested_model,
                        "exclusive: switching models"
                    );
                    current_server.set_state(ServerState::Stopping).await;
                    if let Err(e) = lifecycle::stop_backend(current_server).await {
                        error!(error = %e, "failed to stop current backend during model switch");
                    }
                    current_server.set_state(ServerState::Stopped).await;
                    current_server.stop_notify.notify_waiters();
                }
            }
        }
        // Update active model
        let mut active_write = active.write().await;
        *active_write = Some(requested_model.clone());
    }

    if let Err(msg) = ensure_running(&server).await {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &msg));
    }

    server.touch().await;
    let rebuilt = Request::from_parts(parts, Full::new(body_bytes));
    forward_request(&server, rebuilt).await
}

/// Handle GET/HEAD/etc on multi-route listeners
async fn handle_bodyless_multi<B>(
    routes: &HashMap<String, Arc<ManagedServer>>,
    req: &Request<B>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    let path = req.uri().path();
    if path == "/v1/models" || path == "/v1/models/" {
        return Ok(models_response(routes));
    }
    Ok(error_response(
        StatusCode::BAD_REQUEST,
        "Multi-model endpoint requires a request body with 'model' field",
    ))
}

fn has_body(method: &hyper::Method) -> bool {
    matches!(
        *method,
        hyper::Method::POST | hyper::Method::PUT | hyper::Method::PATCH
    )
}

fn extract_model(body: &Bytes) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("model")?.as_str().map(|s| s.to_string())
}

fn unknown_model_response(
    model: &str,
    routes: &HashMap<String, Arc<ManagedServer>>,
) -> Response<BoxBody<Bytes, BoxError>> {
    let available: Vec<&str> = routes.keys().map(|k| k.as_str()).collect();
    error_response(
        StatusCode::NOT_FOUND,
        &format!(
            "Unknown model '{}'. Available: {}",
            model,
            available.join(", ")
        ),
    )
}

fn models_response(
    routes: &HashMap<String, Arc<ManagedServer>>,
) -> Response<BoxBody<Bytes, BoxError>> {
    let models: Vec<String> = routes
        .keys()
        .map(|name| {
            format!(
                "{{\"id\":\"{}\",\"object\":\"model\",\"owned_by\":\"local\"}}",
                name
            )
        })
        .collect();
    let body = format!("{{\"object\":\"list\",\"data\":[{}]}}", models.join(","));

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|e| -> BoxError { Box::new(e) })
                .boxed(),
        )
        .unwrap()
}

async fn ensure_running(server: &Arc<ManagedServer>) -> Result<(), String> {
    loop {
        let state = server.get_state().await;
        match state {
            ServerState::Running => return Ok(()),
            ServerState::Stopped => {
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
                info!(server = %server.config.name, "waiting for backend to finish starting");
                server.startup_notify.notified().await;
            }
            ServerState::Stopping => {
                warn!(server = %server.config.name, "backend is stopping, waiting before restart");
                server.stop_notify.notified().await;
            }
        }
    }
}

async fn forward_request<B>(
    server: &Arc<ManagedServer>,
    req: Request<B>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error>
where
    B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    let client = Client::builder(TokioExecutor::new()).build_http::<B>();

    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let uri = format!("http://{}{}", server.config.backend, path);

    let mut builder = Request::builder().method(req.method()).uri(&uri);

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
