use axum::{
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
            axum::Json(ErrorBody {
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

/// Axum's `Query`, with a missing or wrong parameter as a plain 400 the
/// dashboard can show, instead of axum's own text.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T, S> axum::extract::FromRequestParts<S> for Query<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Query(v)),
            Err(e) => Err(ApiError::bad_request(query_message(&e.body_text()))),
        }
    }
}

/// axum's "Failed to deserialize query string: missing field `team`", in
/// plain words.
fn query_message(text: &str) -> String {
    let quoted = |after: &str| {
        let rest = &text[text.find(after)? + after.len()..];
        rest.split('`').next().map(str::to_owned)
    };
    if let Some(name) = quoted("missing field `") {
        return format!("Add the {name} parameter.");
    }
    if let Some(value) = quoted("unknown variant `") {
        return format!("\"{value}\" isn't a value this parameter takes.");
    }
    "A parameter in the address isn't valid.".to_owned()
}

/// Axum's `Json`, with a body it can't read as a plain 400 the dashboard
/// can show, instead of axum's own text and 422. As a response, the same.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for Json<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Json(v)),
            Err(e) => Err(json_rejection(&e)),
        }
    }
}

/// An optional body: none without a JSON Content-Type, as axum's.
impl<T, S> axum::extract::OptionalFromRequest<S> for Json<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(
        req: axum::extract::Request,
        state: &S,
    ) -> Result<Option<Self>, ApiError> {
        match <axum::Json<T> as axum::extract::OptionalFromRequest<S>>::from_request(req, state)
            .await
        {
            Ok(v) => Ok(v.map(|axum::Json(v)| Json(v))),
            Err(e) => Err(json_rejection(&e)),
        }
    }
}

fn json_rejection(e: &axum::extract::rejection::JsonRejection) -> ApiError {
    let status = match e.status() {
        StatusCode::UNPROCESSABLE_ENTITY => StatusCode::BAD_REQUEST,
        s => s,
    };
    ApiError::new(status, json_message(&e.body_text()))
}

impl<T: serde::Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// axum's "Failed to deserialize the JSON body into the target type:
/// missing field `template` at line 1 column 2", in plain words.
fn json_message(text: &str) -> String {
    let quoted = |after: &str| {
        let rest = &text[text.find(after)? + after.len()..];
        rest.split('`').next().map(str::to_owned)
    };
    if let Some(name) = quoted("missing field `") {
        return format!("Add the {name} field.");
    }
    if let Some(name) = quoted("unknown field `") {
        return format!("There's no field named {name}.");
    }
    if let Some(value) = quoted("unknown variant `") {
        return format!("\"{value}\" isn't a value it takes.");
    }
    if text.contains("Content-Type") {
        return "Send the body as JSON, with Content-Type: application/json.".to_owned();
    }
    if text.starts_with("Failed to parse") || text.contains("EOF while parsing") {
        return "The body isn't valid JSON.".to_owned();
    }
    // "…target type: cpus: invalid type: …": the field it's about.
    if let Some(rest) = text.split_once("target type: ").map(|(_, r)| r)
        && let Some((path, _)) = rest.split_once(": ")
        && !path.contains(' ')
        && path != "."
    {
        return format!("The {path} field isn't valid.");
    }
    if text.contains("length limit") {
        return "The body is too large.".to_owned();
    }
    "The body isn't valid.".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_errors_in_plain_words() {
        assert_eq!(
            query_message("Failed to deserialize query string: missing field `team`"),
            "Add the team parameter."
        );
        assert_eq!(
            query_message(
                "Failed to deserialize query string: unknown variant `x`, expected `a` or `b`"
            ),
            "\"x\" isn't a value this parameter takes."
        );
        assert_eq!(
            query_message("Failed to deserialize query string: invalid digit found in string"),
            "A parameter in the address isn't valid."
        );
    }

    #[test]
    fn body_errors_in_plain_words() {
        let t = "Failed to deserialize the JSON body into the target type: ";
        assert_eq!(
            json_message(&format!("{t}missing field `template` at line 1 column 2")),
            "Add the template field."
        );
        assert_eq!(
            json_message(&format!(
                "{t}unknown field `x`, expected one of `a`, `b` at line 1 column 4"
            )),
            "There's no field named x."
        );
        assert_eq!(
            json_message(&format!(
                "{t}cpus: invalid type: string \"a\", expected u32 at line 1 column 11"
            )),
            "The cpus field isn't valid."
        );
        assert_eq!(
            json_message(
                "Failed to parse the request body as JSON: EOF while parsing an object at line 1 column 1"
            ),
            "The body isn't valid JSON."
        );
        assert_eq!(
            json_message("Expected request with `Content-Type: application/json`"),
            "Send the body as JSON, with Content-Type: application/json."
        );
    }

    #[tokio::test]
    async fn a_body_it_cant_read_is_a_plain_400() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct B {
            template: String,
        }
        let req = axum::http::Request::post("/x")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let e = <Json<B> as axum::extract::FromRequest<()>>::from_request(req, &())
            .await
            .err()
            .unwrap();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.message, "Add the template field.");
    }

    #[tokio::test]
    async fn a_missing_parameter_is_a_plain_400() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Q {
            team: String,
        }
        let req = axum::http::Request::get("/x?other=1").body(()).unwrap();
        let (mut parts, ()) = req.into_parts();
        let e =
            <Query<Q> as axum::extract::FromRequestParts<()>>::from_request_parts(&mut parts, &())
                .await
                .err()
                .unwrap();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.message, "Add the team parameter.");
    }
}
