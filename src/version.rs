pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("M2K_BUILD_COMMIT");
pub const DISPLAY: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("M2K_BUILD_COMMIT"),
    ")"
);
