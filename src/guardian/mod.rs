//! Guardian-derived review orchestration; host execution stays outside this module.
pub mod assessment;
mod host;
mod transport;
pub(crate) use host::PendingReview;

pub use transport::{ReviewConfig, review, status};

pub fn legacy_reviewer() -> String {
    "user".into()
}
pub fn default_reviewer() -> String {
    "auto_review".into()
}
