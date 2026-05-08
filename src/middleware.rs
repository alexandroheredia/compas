use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use std::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

pub async fn request_logger(req: Request<Body>, next: Next) -> Response<Body> {
    let request_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let start = Instant::now();

    info!(
        request_id = %request_id,
        method = %method,
        path = %path,
        "request started"
    );

    let mut response = next.run(req).await;

    let duration = start.elapsed();
    let status = response.status();

    // Inject request-id header into response
    response
        .headers_mut()
        .insert("x-request-id", request_id.parse().unwrap());

    if status.is_server_error() || status.is_client_error() {
        warn!(
            request_id = %request_id,
            method = %method,
            path = %path,
            status = %status,
            duration_ms = %duration.as_millis(),
            "request failed"
        );
    } else {
        info!(
            request_id = %request_id,
            method = %method,
            path = %path,
            status = %status,
            duration_ms = %duration.as_millis(),
            "request completed"
        );
    }

    response
}
