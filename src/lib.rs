//! crumb: a Matrix push gateway that forwards to Apple's APNs.
//!
//! Spec: <https://spec.matrix.org/latest/push-gateway-api/>.

pub mod apns;
pub mod config;
pub mod dedupe;
pub mod dispatch;
pub mod jwt;
pub mod log;
pub mod matrix;
pub mod notification;
pub mod payload;
pub mod server;
pub mod token;
