
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod core;
pub mod oapi;
mod prometheus;
mod ods;
mod args;