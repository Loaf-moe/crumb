//! An APNs client: one persistent HTTP/2 connection per app, with every push
//! multiplexed over it as a stream.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::AUTHORIZATION;
use http::{HeaderName, HeaderValue, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use serde::Deserialize;
use tracing::warn;

use crate::config::{AppConfig, Origin};
use crate::jwt::{SystemClock, TokenSource};
use crate::log::Pk;
use crate::payload::ApnsRequest;
use crate::token::DeviceToken;

const APNS_TOPIC: HeaderName = HeaderName::from_static("apns-topic");
const APNS_PUSH_TYPE: HeaderName = HeaderName::from_static("apns-push-type");
const APNS_PRIORITY: HeaderName = HeaderName::from_static("apns-priority");
const APNS_EXPIRATION: HeaderName = HeaderName::from_static("apns-expiration");
const APNS_COLLAPSE_ID: HeaderName = HeaderName::from_static("apns-collapse-id");
const APNS_ID: HeaderName = HeaderName::from_static("apns-id");
const ALERT: HeaderValue = HeaderValue::from_static("alert");

/// How long APNs keeps trying to reach an offline phone.
const EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);
/// Pings that notice a dead idle connection before a push has to.
const KEEP_ALIVE: Duration = Duration::from_secs(60);
/// How often one push may be re-sent after APNs refuses its stream. A new
/// connection's first burst can outrun the server's stream limit; once its
/// SETTINGS arrive, h2 queues streams over the limit itself.
const MAX_REFUSALS: u32 = 3;

/// What became of one push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// The token is dead for good: the homeserver must drop the pusher.
    Rejected,
    /// Worth retrying later.
    Transient,
    /// Undeliverable as sent, but the token is fine: retrying can't help.
    Dropped,
}

/// APNs's answer to one request, before crumb decides what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Delivered,
    Rejected,
    /// The provider token is stale: renew it and send once more.
    RenewToken,
    Transient,
    /// APNs refused the payload itself (`413`).
    Dropped,
}

/// The `reason` APNs gives with an error. Only the reasons crumb acts on are
/// named; the rest are transient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Reason {
    BadDeviceToken,
    DeviceTokenNotForTopic,
    Unregistered,
    ExpiredProviderToken,
    InvalidProviderToken,
    #[serde(other)]
    Other,
}

impl From<(StatusCode, Option<Reason>)> for Verdict {
    fn from((status, reason): (StatusCode, Option<Reason>)) -> Self {
        match (status, reason) {
            (StatusCode::OK, _) => Verdict::Delivered,
            (StatusCode::PAYLOAD_TOO_LARGE, _) => Verdict::Dropped,
            (StatusCode::GONE, _)
            | (
                StatusCode::BAD_REQUEST,
                Some(Reason::BadDeviceToken | Reason::DeviceTokenNotForTopic),
            ) => Verdict::Rejected,
            (
                StatusCode::FORBIDDEN,
                Some(Reason::ExpiredProviderToken | Reason::InvalidProviderToken),
            ) => Verdict::RenewToken,
            _ => Verdict::Transient,
        }
    }
}

impl From<Verdict> for Outcome {
    /// Once a renewal has been tried, a stale token is just transient.
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Delivered => Outcome::Delivered,
            Verdict::Rejected => Outcome::Rejected,
            Verdict::RenewToken | Verdict::Transient => Outcome::Transient,
            Verdict::Dropped => Outcome::Dropped,
        }
    }
}

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct ApnsClient {
    http: HttpClient,
    origin: Origin,
    topic: HeaderValue,
    tokens: TokenSource,
}

impl From<AppConfig> for ApnsClient {
    fn from(app: AppConfig) -> Self {
        let tokens = TokenSource::new(app.key, app.key_id, app.team_id, Arc::new(SystemClock));
        ApnsClient::new(app.endpoint.into(), app.topic.into(), tokens)
    }
}

impl ApnsClient {
    pub fn new(origin: Origin, topic: HeaderValue, tokens: TokenSource) -> Self {
        // HTTP/2 only, as APNs requires. Plain HTTP is allowed so a test can
        // reach a fake APNs; an Origin from the config is always HTTPS.
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http2()
            .build();
        let http = Client::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .http2_only(true)
            // APNs wants one long-lived connection. The pool's default
            // reaps it after 90 s idle, and keep-alive pings don't count
            // as use, so a quiet spell would cost a fresh handshake.
            .pool_idle_timeout(None)
            .http2_keep_alive_interval(KEEP_ALIVE)
            .http2_keep_alive_while_idle(true)
            .build(connector);
        Self {
            http,
            origin,
            topic,
            tokens,
        }
    }

    pub async fn send(&self, token: DeviceToken<'_>, request: &ApnsRequest) -> Outcome {
        let mut renewed = false;
        let mut refusals = 0;
        loop {
            let bearer = if renewed {
                self.tokens.renew()
            } else {
                self.tokens.bearer()
            };
            match self.post(token, request, bearer).await {
                Ok(Verdict::RenewToken) if !renewed => renewed = true,
                Ok(verdict) => return verdict.into(),
                // A refused stream was never processed, so sending it again
                // can't notify twice (RFC 9113, section 8.7).
                Err(e) if e.is_refused_stream() && refusals < MAX_REFUSALS => refusals += 1,
                Err(e) => {
                    warn!(pk = %Pk(token.as_str()), error = %e, "APNs request failed");
                    return Outcome::Transient;
                }
            }
        }
    }

    async fn post(
        &self,
        token: DeviceToken<'_>,
        request: &ApnsRequest,
        bearer: HeaderValue,
    ) -> Result<Verdict, SendError> {
        let uri = Uri::builder()
            .scheme(self.origin.scheme.clone())
            .authority(self.origin.authority.clone())
            .path_and_query(format!("/3/device/{token}"))
            .build()?;
        let mut builder = Request::post(uri)
            .header(AUTHORIZATION, bearer)
            .header(APNS_TOPIC, self.topic.clone())
            .header(APNS_PUSH_TYPE, ALERT)
            .header(APNS_PRIORITY, HeaderValue::from(request.priority))
            .header(APNS_EXPIRATION, expiration());
        if let Some(id) = &request.collapse_id {
            builder = builder.header(APNS_COLLAPSE_ID, HeaderValue::from(id.clone()));
        }
        let response = self
            .http
            .request(builder.body(Full::new(request.body.clone()))?)
            .await?;

        let status = response.status();
        if status == StatusCode::OK {
            return Ok(Verdict::Delivered);
        }
        let apns_id = response.headers().get(APNS_ID).cloned();
        let body = response.into_body().collect().await?.to_bytes();
        let reason = serde_json::from_slice::<ErrorBody>(&body)
            .ok()
            .map(|b| b.reason);
        let verdict = Verdict::from((status, reason));
        warn!(
            pk = %Pk(token.as_str()),
            status = status.as_u16(),
            ?reason,
            apns_id = ?apns_id,
            ?verdict,
            "APNs refused a push"
        );
        Ok(verdict)
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    reason: Reason,
}

/// `apns-expiration`, as Unix seconds.
fn expiration() -> HeaderValue {
    let at = SystemTime::now() + EXPIRY;
    HeaderValue::from(at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

#[derive(Debug)]
enum SendError {
    Request(http::Error),
    Connection(hyper_util::client::legacy::Error),
    Body(hyper::Error),
}

impl SendError {
    fn is_refused_stream(&self) -> bool {
        let SendError::Connection(e) = self else {
            return false;
        };
        let mut source = std::error::Error::source(e);
        while let Some(e) = source {
            if let Some(h2) = e.downcast_ref::<h2::Error>() {
                return h2.reason() == Some(h2::Reason::REFUSED_STREAM);
            }
            source = e.source();
        }
        false
    }
}

impl From<http::Error> for SendError {
    fn from(e: http::Error) -> Self {
        SendError::Request(e)
    }
}

impl From<hyper_util::client::legacy::Error> for SendError {
    fn from(e: hyper_util::client::legacy::Error) -> Self {
        SendError::Connection(e)
    }
}

impl From<hyper::Error> for SendError {
    fn from(e: hyper::Error) -> Self {
        SendError::Body(e)
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Request(e) => write!(f, "building the request: {e}"),
            SendError::Connection(e) => {
                write!(f, "connection: {e}")?;
                let mut source = std::error::Error::source(e);
                while let Some(e) = source {
                    write!(f, ": {e}")?;
                    source = e.source();
                }
                Ok(())
            }
            SendError::Body(e) => write!(f, "reading the response: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_follow_apns_status_and_reason() {
        use Reason::*;
        let cases = [
            (200, None, Verdict::Delivered),
            (410, Some(Unregistered), Verdict::Rejected),
            (410, None, Verdict::Rejected),
            (400, Some(BadDeviceToken), Verdict::Rejected),
            (400, Some(DeviceTokenNotForTopic), Verdict::Rejected),
            (400, Some(Other), Verdict::Transient),
            (400, None, Verdict::Transient),
            (403, Some(ExpiredProviderToken), Verdict::RenewToken),
            (403, Some(InvalidProviderToken), Verdict::RenewToken),
            (403, Some(Other), Verdict::Transient),
            (429, Some(Other), Verdict::Transient),
            (500, None, Verdict::Transient),
            (503, Some(Other), Verdict::Transient),
            (413, Some(Other), Verdict::Dropped),
            (413, None, Verdict::Dropped),
        ];
        for (status, reason, verdict) in cases {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                Verdict::from((status, reason)),
                verdict,
                "{status} {reason:?}"
            );
        }
    }

    #[test]
    fn unknown_reasons_parse_as_other() {
        let body: ErrorBody = serde_json::from_str(r#"{"reason":"TooManyRequests"}"#).unwrap();
        assert_eq!(body.reason, Reason::Other);
        let body: ErrorBody =
            serde_json::from_str(r#"{"reason":"Unregistered","timestamp":1}"#).unwrap();
        assert_eq!(body.reason, Reason::Unregistered);
    }

    #[test]
    fn a_stale_token_after_renewal_is_transient() {
        assert_eq!(Outcome::from(Verdict::RenewToken), Outcome::Transient);
    }

    #[test]
    fn a_payload_apns_refuses_as_too_large_is_dropped_not_retried() {
        assert_eq!(Outcome::from(Verdict::Dropped), Outcome::Dropped);
    }
}
