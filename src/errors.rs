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
    #[error("form security token expired or changed")]
    Csrf,
    #[error("request origin could not be verified")]
    Origin,
    #[error("server is busy")]
    Busy,
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    RateLimited(String),
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Anyhow(anyhow::Error),
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
            Self::Busy => (StatusCode::SERVICE_UNAVAILABLE, "The server is busy. Please retry shortly.".to_owned()),
            Self::Origin => (StatusCode::FORBIDDEN, "This request could not be verified. Open RustPost directly and submit the form again.".to_owned()),
            Self::Csrf => (StatusCode::FORBIDDEN, "This form has expired or belongs to a different sign-in. Your changes were not saved. Save any unsaved text, reload the page, and try again.".to_owned()),
            Self::PayloadTooLarge(message) => (StatusCode::PAYLOAD_TOO_LARGE, message),
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
        let mut response =
            (status, Html(crate::render::error_page(status, &message))).into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        response.headers_mut().insert(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        );
        response
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        if error
            .downcast_ref::<crate::auth::PasswordWorkBusy>()
            .is_some()
        {
            Self::Busy
        } else {
            Self::Anyhow(error)
        }
    }
}
