//! Host-side egress: an allowlist policy and a `CONNECT` proxy that enforces it.
//!
//! The sandbox never talks to the network directly. It hands each `CONNECT
//! host:port` request to [`EgressProxy`], which applies [`EgressPolicy`],
//! resolves the name on the host, refuses non-public addresses, and only then
//! opens the TCP connection.

mod policy;
mod proxy;

pub use policy::{DEFAULT_PORTS, EgressPolicy};
pub use proxy::{
    AddressClass, Connector, EgressEvent, EgressProxy, Resolver, SystemResolver, TcpConnector,
    classify_address,
};
