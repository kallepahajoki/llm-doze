use std::collections::HashMap;
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

pub enum ListenerRouter {
    Single(Arc<ManagedServer>),
    Multi {
        routes: HashMap<String, Arc<ManagedServer>>,
    },
}

pub async fn handle_request(
    router: Arc<ListenerRouter>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
    match &*router {
        ListenerRouter::Single(server) => handle_single(Arc::clone(server), req).await,
        ListenerRouter::Multi { routes } => handle_multi(routes, req).await,
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
    // For GET/HEAD/OPTIONS/DELETE, we can't extract a model from the body.
    // Return a list of available models for GET /v1/models, otherwise 400.
    if !req.method().is_idempotent() || req.method() == hyper::Method::POST {
        // Methods that have a body: POST, PUT, PATCH
    } else {
        // GET, HEAD, OPTIONS, DELETE — no body to parse
        // Handle /v1/models as a special case
        let path = req.uri().path();
        if path == "/v1/models" || path == "/v1/models/" {
            return Ok(models_response(routes));
        }
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "Multi-model endpoint requires a request body with 'model' field",
        ));
    }

    // Check auth against first route's token (all routes on same listener share auth context)
    // The auth token is resolved at the listener level for multi-route
    let first_server = routes.values().next().unwrap();
    if let Err(resp) = auth::check_auth(&req, first_server.auth_token.as_deref()) {
        return Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()));
    }

    // Buffer body
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

    // Extract model from JSON
    let model_name = match extract_model(&body_bytes) {
        Some(name) => name,
        None => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Missing or invalid 'model' field in request body",
            ));
        }
    };

    // Find matching route
    let server = match routes.get(&model_name) {
        Some(s) => Arc::clone(s),
        None => {
            let available: Vec<&str> = routes.keys().map(|k| k.as_str()).collect();
            return Ok(error_response(
                StatusCode::NOT_FOUND,
                &format!(
                    "Unknown model '{}'. Available: {}",
                    model_name,
                    available.join(", ")
                ),
            ));
        }
    };

    if let Err(msg) = ensure_running(&server).await {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &msg));
    }

    server.touch().await;

    // Rebuild request with buffered body
    let rebuilt = Request::from_parts(parts, Full::new(body_bytes));
    forward_request(&server, rebuilt).await
}

fn extract_model(body: &Bytes) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("model")?.as_str().map(|s| s.to_string())
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
