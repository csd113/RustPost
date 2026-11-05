//! Aggregate HTTP counters; no paths, headers, user IDs or body data retained.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};

#[derive(Default)]
pub struct Requests {
    total: AtomicU64,
    in_flight: AtomicU64,
    server_errors: AtomicU64,
}
impl Requests {
    pub fn summary(&self) -> String {
        format!(
            "HTTP handlers: total {} | in-flight {} | 5xx responses {}",
            self.total.load(Ordering::Relaxed),
            self.in_flight.load(Ordering::Relaxed),
            self.server_errors.load(Ordering::Relaxed)
        )
    }
}
struct InFlight(Arc<Requests>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn observe(
    State(requests): State<Arc<Requests>>,
    request: Request,
    next: Next,
) -> Response {
    requests.total.fetch_add(1, Ordering::Relaxed);
    requests.in_flight.fetch_add(1, Ordering::Relaxed);
    let _guard = InFlight(requests.clone());
    let response = next.run(request).await;
    if response.status().is_server_error() {
        requests.server_errors.fetch_add(1, Ordering::Relaxed);
    }
    response
}
