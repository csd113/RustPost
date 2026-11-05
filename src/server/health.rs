//! Minimal, unauthenticated process health. No paths, counts or configuration are exposed.

use super::*;

pub(super) async fn live() -> Response {
    response(StatusCode::OK, "alive")
}

pub(super) async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if backup::operation_in_progress(&state.paths) {
        return response(StatusCode::SERVICE_UNAVAILABLE, "not-ready");
    }
    let check = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.pool.call(|conn| {
            let value: i64 = conn.query_row("SELECT 1", [], |r| r.get(0))?;
            anyhow::ensure!(value == 1, "database unavailable");
            Ok(())
        }),
    )
    .await;
    if matches!(check, Ok(Ok(()))) {
        response(StatusCode::OK, "ready")
    } else {
        response(StatusCode::SERVICE_UNAVAILABLE, "not-ready")
    }
}

fn response(status: StatusCode, value: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"status":value})),
    )
        .into_response()
}
