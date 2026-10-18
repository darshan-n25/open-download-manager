pub mod client;
pub mod proxy;

pub use client::NetClient;
pub use proxy::{parse_proxy, ProxySpec};
