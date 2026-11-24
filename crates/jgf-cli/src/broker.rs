//! In-lab client entry point for the host-owned GitHub broker.

use jgf_hub::{BrokerClient, BrokerClientError};

/// Creates a broker client from `JG_BROKER_SOCKET` and `JG_BROKER_TOKEN`.
///
/// The token is consumed by the client protocol and should never be logged.
pub fn client_from_env() -> Result<BrokerClient, BrokerClientError> {
    BrokerClient::from_env()
}
