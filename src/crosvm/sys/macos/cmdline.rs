// macOS command-line subcommands for crosvm.

use argh::FromArgs;

#[derive(FromArgs)]
#[argh(subcommand)]
/// macOS Devices
pub enum DeviceSubcommand {}

#[derive(FromArgs)]
#[argh(subcommand)]
/// macOS Commands
pub enum Commands {}
