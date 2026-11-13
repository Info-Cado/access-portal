use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// One error type for handlers. Anything that goes wrong becomes a 500 and is logged;
/// we never leak internals to the browser.
pub struct AppError(pub anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Something went wrong</h1><p>Please try again.</p>".to_string()),
        )
            .into_response()
    }
}
