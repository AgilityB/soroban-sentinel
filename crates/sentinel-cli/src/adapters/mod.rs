//! External service and notification adapters for `sentinel-cli`.

pub mod alert_mattermost;

pub use alert_mattermost::{
    MattermostAlertDispatcher, MattermostAttachment, MattermostError, MattermostField,
    MattermostPayload, MattermostProps, COLOR_CRITICAL, COLOR_WARN,
};
pub mod alert_teams;
