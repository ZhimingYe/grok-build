#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#![deny(clippy::indexing_slicing)]
pub mod auto_update;
mod cleanup_downloads;
pub mod version;
mod version_policy;
mod winget;
pub use auto_update::UpdateStatus;
pub use version::{UpdateConfig, channel_label, channel_name, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;
// ZMY SELF-ADDED CODE START

pub const UPDATES_DISABLED_MESSAGE: &str =
    "Built-in updates and update checks are disabled in this fork. Install releases from ZhimingYe/grok-build manually.";

pub const fn updates_enabled() -> bool {
    false
}
// ZMY SELF-ADDED CODE END
