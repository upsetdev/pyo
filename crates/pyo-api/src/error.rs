use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// An error the dashboard can show: a status and one sentence.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Seconds, sent as Retry-After (429s from ratelimit.rs).
    pub retry_after: Option<u64>,
    /// Asks the caller to confirm and send the operation again with
    /// `confirm: true` (a very large replica change).
    pub needs_confirm: bool,
}

pub type ApiResult<T> = Result<T, ApiError>;

/// What every error response carries: one sentence to show.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    pub error: String,
    /// The change wasn't made: confirm it with the user, then send it again
    /// with `confirm: true`.
    #[serde(rename = "needsConfirm", skip_serializing_if = "std::ops::Not::not")]
    pub needs_confirm: bool,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
            needs_confirm: false,
        }
    }
    pub fn with_retry_after(self, secs: u64) -> Self {
        Self {
            retry_after: Some(secs),
            ..self
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Sign in to continue.")
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("{what} not found."))
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    /// A 409 that goes through once the caller confirms it.
    pub fn needs_confirm(message: impl Into<String>) -> Self {
        Self {
            needs_confirm: true,
            ..Self::conflict(message)
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (
            self.status,
            Json(ErrorBody {
                error: self.message,
                needs_confirm: self.needs_confirm,
            }),
        )
            .into_response();
        if let Some(secs) = self.retry_after {
            r.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, secs.into());
        }
        r
    }
}

/// Database and other unexpected errors: logged in full, shown generically.
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e
            && db.is_unique_violation()
        {
            return Self::conflict("That name is already taken.");
        }
        tracing::error!(error = %e, "database error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong on our side.",
        )
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong on our side.",
        )
    }
}

pub async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "No such endpoint.")
}
