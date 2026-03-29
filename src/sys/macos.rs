// macOS platform entry point for crosvm.

pub(crate) mod main;
mod panic_hook;

pub(crate) use panic_hook::set_panic_hook;
