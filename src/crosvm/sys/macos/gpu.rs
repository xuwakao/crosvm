// macOS GPU render server parameters stub.
// macOS does not support a separate GPU render server process.
// This type exists to satisfy the shared Config struct that references it.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

#[derive(Clone, Debug, Serialize, Deserialize, FromKeyValues, PartialEq, Eq)]
pub struct GpuRenderServerParameters {
    pub path: PathBuf,
    pub cache_path: Option<String>,
    pub cache_size: Option<String>,
    pub foz_db_list_path: Option<String>,
    pub precompiled_cache_path: Option<String>,
    pub ld_preload_path: Option<String>,
}
