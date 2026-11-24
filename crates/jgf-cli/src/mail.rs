//! In-lab client entry point for the host-owned message shim.

use jgf_hub::{HubClient, HubClientError};

/// Creates a message client from `JG_HUB_SOCKET` and `JG_HUB_TOKEN`.
///
/// The token is consumed by the client protocol and should never be logged.
pub fn client_from_env() -> Result<HubClient, HubClientError> {
    HubClient::from_env()
}
