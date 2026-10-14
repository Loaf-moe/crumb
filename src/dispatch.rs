//! One notify request: fan out to every device, then answer the homeserver.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::response::{IntoResponse, Response};
use futures_util::future::join_all;
use tracing::{debug, info, warn};

use crate::apns::{ApnsClient, Outcome};
use crate::dedupe::Dedupe;
use crate::log::Pk;
use crate::matrix::{MatrixError, NotifyResponse};
use crate::notification::{Device, Notification, Push};
use crate::payload::Requests;
use crate::token::DeviceToken;

pub struct Gateway {
    apps: HashMap<String, ApnsClient>,
    dedupe: Dedupe,
    /// The most a homeserver waits for an answer.
    deadline: Duration,
}

/// The answer to a notify request.
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Every device was settled; these pushkeys are dead.
    Done { rejected: Vec<String> },
    /// At least one device may still get it on a retry.
    Retry,
}

impl IntoResponse for Dispatch {
    fn into_response(self) -> Response {
        match self {
            Dispatch::Done { rejected } => NotifyResponse { rejected }.into_response(),
            Dispatch::Retry => MatrixError::retry().into_response(),
        }
    }
}

impl Gateway {
    pub fn new(apps: HashMap<String, ApnsClient>, dedupe: Dedupe, deadline: Duration) -> Self {
        Self {
            apps,
            dedupe,
            deadline,
        }
    }

    pub async fn notify(&self, notification: Notification) -> Dispatch {
        let Notification {
            push,
            priority,
            badge,
            devices,
        } = notification;
        let requests = Requests::new(&push, priority, badge);

        // Every device is sent concurrently from this one task: the futures
        // borrow the notification, and the shared HTTP/2 connection carries
        // them as parallel streams.
        let sends = devices.iter().map(|d| self.deliver(&push, &requests, d));
        let Ok(outcomes) = tokio::time::timeout(self.deadline, join_all(sends)).await else {
            warn!(devices = devices.len(), "APNs missed the deadline");
            return Dispatch::Retry;
        };

        let mut rejected = Vec::new();
        let mut retry = false;
        for (device, outcome) in devices.into_iter().zip(outcomes) {
            match outcome {
                Outcome::Delivered | Outcome::Dropped => {}
                Outcome::Rejected => rejected.push(device.pushkey),
                Outcome::Transient => retry = true,
            }
        }
        if retry {
            Dispatch::Retry
        } else {
            Dispatch::Done { rejected }
        }
    }

    /// `app_id` comes from an unauthenticated caller, so it is logged with
    /// `?`: Debug escapes newlines that would otherwise forge log lines.
    async fn deliver(&self, push: &Push, requests: &Requests<'_>, device: &Device) -> Outcome {
        let pk = Pk(&device.pushkey);
        let Some(client) = self.apps.get(&device.app_id) else {
            info!(%pk, app_id = ?device.app_id, "unknown app_id, rejected");
            return Outcome::Rejected;
        };
        let Ok(token) = DeviceToken::try_from(device.pushkey.as_str()) else {
            info!(%pk, app_id = ?device.app_id, "pushkey is not a device token, rejected");
            return Outcome::Rejected;
        };
        let event_id = push.event_id();
        if let Some(event_id) = event_id
            && self.dedupe.contains(event_id, token, Instant::now())
        {
            debug!(%pk, "already delivered");
            return Outcome::Delivered;
        }

        let outcome = client.send(token, &requests.for_device(device.sound)).await;
        if outcome == Outcome::Delivered
            && let Some(event_id) = event_id
        {
            self.dedupe.insert(event_id, token, Instant::now());
        }
        info!(%pk, app_id = ?device.app_id, ?outcome, "push");
        outcome
    }
}
