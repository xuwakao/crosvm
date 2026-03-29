// macOS vsock stub — vsock uses vhost-vsock on Linux, this is a placeholder.
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Serialize, Deserialize)]
pub struct VsockConfig {
    pub cid: u64,
}

pub struct Vsock;
