//! Network plumbing: client construction, TLS policy, proxy resolution, and the
//! per-server connection ceiling.

pub mod client;
pub mod limits;
pub mod proxy;

pub use client::NetClient;
pub use limits::{host_of, ServerLimits};
pub use proxy::{parse_proxy, ProxySpec};
