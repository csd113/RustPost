use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("{0}")]
    RateLimited(String),
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication required".to_owned(),
            ),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_owned()),
            Self::RateLimited(message) => (StatusCode::TOO_MANY_REQUESTS, message),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            Self::Io(_) | Self::Sqlite(_) | Self::Anyhow(_) => {
                tracing::error!(error = ?self, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_owned(),
                )
            }
        };
        (status, Html(crate::render::error_page(status, &message))).into_response()
    }
}
