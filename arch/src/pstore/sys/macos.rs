// macOS pstore stub
use std::fs::OpenOptions;

pub fn set_extra_open_opts(_opts: &mut OpenOptions) {
    // No special options needed on macOS.
}
