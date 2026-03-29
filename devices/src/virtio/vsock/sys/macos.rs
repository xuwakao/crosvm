// macOS vsock stub — vsock uses vhost-vsock on Linux, this is a placeholder.
use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, FromKeyValues)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct VsockConfig {
    pub cid: u64,
}

impl VsockConfig {
    pub fn new(cid: u64) -> Self {
        Self { cid }
    }
}

pub struct Vsock;
