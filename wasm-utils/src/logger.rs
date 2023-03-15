#![cfg(feature = "cli")]

use env_logger::Builder;
use log::LevelFilter;
use std::sync::Once;

/// Intializes log with default settings.
pub fn init() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        Builder::new()
            .filter(None, LevelFilter::Info)
            .parse_env("RUST_LOG")
            .init();

        log::trace!("logger initialized");
    })
}
