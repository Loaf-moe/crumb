//! The Push Gateway API's request, response and error bodies.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::error::Category;

use crate::notification::Notification;

/// The body of `POST /_matrix/push/v1/notify`.
#[derive(Debug, Deserialize)]
pub struct NotifyRequest {
    pub notification: Notification,
}

impl TryFrom<&[u8]> for NotifyRequest {
    type Error = MatrixError;

    /// One pass over the body. serde_json's error category tells "not JSON"
    /// (`M_NOT_JSON`) apart from "the wrong JSON" (`M_BAD_JSON`).
    fn try_from(body: &[u8]) -> Result<Self, Self::Error> {
        serde_json::from_slice(body).map_err(|e| match e.classify() {
            Category::Syntax | Category::Eof | Category::Io => MatrixError::not_json(),
            Category::Data => MatrixError::bad_json(e.to_string()),
        })
    }
}

#[derive(Debug, Serialize)]
pub struct NotifyResponse {
    /// Pushkeys the homeserver must stop using and whose pushers it must
    /// remove.
    pub rejected: Vec<String>,
}

impl IntoResponse for NotifyResponse {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

/// The standard Matrix error: a status plus `{"errcode", "error"}`.
#[derive(Debug, PartialEq, Eq)]
pub struct MatrixError {
    pub status: StatusCode,
    pub errcode: &'static str,
    pub error: String,
}

impl MatrixError {
    pub fn not_json() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            errcode: "M_NOT_JSON",
            error: "Content not JSON.".into(),
        }
    }

    pub fn bad_json(reason: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            errcode: "M_BAD_JSON",
            error: reason,
        }
    }

    pub fn unrecognized(status: StatusCode) -> Self {
        Self {
            status,
            errcode: "M_UNRECOGNIZED",
            error: "Unrecognized request".into(),
        }
    }

    /// APNs could not take every push. The spec has the homeserver retry an
    /// HTTP error with backoff; dedupe stops the retry re-pinging devices
    /// that did get it.
    pub fn retry() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            errcode: "M_UNKNOWN",
            error: "APNs unavailable, try again later.".into(),
        }
    }
}

impl IntoResponse for MatrixError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body<'a> {
            errcode: &'a str,
            error: &'a str,
        }
        let body = Body {
            errcode: self.errcode,
            error: &self.error,
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Result<NotifyRequest, MatrixError> {
        NotifyRequest::try_from(body.as_bytes())
    }

    #[test]
    fn a_valid_body_parses() {
        let req = parse(r#"{"notification":{"devices":[{"app_id":"a","pushkey":"00"}]}}"#).unwrap();
        assert_eq!(req.notification.devices.len(), 1);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let body = r#"{"future":1,"notification":{"new":true,"devices":[
            {"app_id":"a","pushkey":"00","pushkey_ts":1,"data":{"format":"event_id_only"},"extra":[]}]}}"#;
        assert!(parse(body).is_ok());
    }

    #[test]
    fn a_body_that_is_not_json_is_m_not_json() {
        for body in ["", "not json", r#"{"notification":"#] {
            assert_eq!(parse(body).unwrap_err().errcode, "M_NOT_JSON", "{body:?}");
        }
    }

    #[test]
    fn the_wrong_json_is_m_bad_json() {
        for body in [
            "{}",
            "[1,2]",
            r#"{"notification":{}}"#,
            r#"{"notification":{"devices":[{"app_id":"a"}]}}"#,
            r#"{"notification":{"prio":"urgent","devices":[]}}"#,
        ] {
            let e = parse(body).unwrap_err();
            assert_eq!(
                (e.status, e.errcode),
                (StatusCode::BAD_REQUEST, "M_BAD_JSON"),
                "{body}"
            );
        }
    }
}
