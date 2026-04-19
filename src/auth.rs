use hyper::{Request, Response, StatusCode};
use http_body_util::Full;
use bytes::Bytes;

pub fn check_auth<B>(req: &Request<B>, expected_token: Option<&str>) -> Result<(), Response<Full<Bytes>>> {
    let expected = match expected_token {
        Some(token) => token,
        None => return Ok(()), // No auth required
    };

    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    match header {
        Some(value) if value.starts_with("Bearer ") => {
            let token = &value[7..];
            if token == expected {
                Ok(())
            } else {
                Err(error_response(StatusCode::UNAUTHORIZED, "Invalid token"))
            }
        }
        Some(_) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Invalid authorization format, expected: Bearer <token>",
        )),
        None => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header",
        )),
    }
}

fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = format!("{{\"error\": \"{}\"}}", message);
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Request;

    #[test]
    fn test_no_auth_required() {
        let req = Request::builder()
            .uri("/test")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(check_auth(&req, None).is_ok());
    }

    #[test]
    fn test_valid_token() {
        let req = Request::builder()
            .uri("/test")
            .header("authorization", "Bearer my-secret")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(check_auth(&req, Some("my-secret")).is_ok());
    }

    #[test]
    fn test_invalid_token() {
        let req = Request::builder()
            .uri("/test")
            .header("authorization", "Bearer wrong-token")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let result = check_auth(&req, Some("my-secret"));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_missing_header() {
        let req = Request::builder()
            .uri("/test")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let result = check_auth(&req, Some("my-secret"));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_wrong_auth_format() {
        let req = Request::builder()
            .uri("/test")
            .header("authorization", "Basic dXNlcjpwYXNz")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let result = check_auth(&req, Some("my-secret"));
        assert!(result.is_err());
    }
}
