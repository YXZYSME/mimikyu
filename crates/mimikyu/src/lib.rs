//! # mimikyu
//!
//! Fast dual-engine (**Chromium CDP** & **Firefox WebDriver BiDi**) browser automation,
//! session/profile isolation, coherent environment profiles, and MCP server runtime
//! built for AI agents.
//!
//! This top-level crate re-exports the complete [`mimikyu_core`] library API so Rust
//! applications and agent frameworks can depend on `mimikyu` directly:
//!
//! ```toml
//! [dependencies]
//! mimikyu = "0.2"
//! ```
//!
//! ## CLI, Warm Daemon & 33-Tool MCP Server
//!
//! Looking for the `mimikyu` command-line binary, background Unix-socket daemon,
//! or stdio Model Context Protocol (`mcp`) server? Install `mimikyu-cli` (or use the
//! signed release installer):
//!
//! ```sh
//! cargo install mimikyu-cli
//! # or prebuilt binary via cargo-binstall:
//! cargo binstall mimikyu-cli
//! ```

pub use mimikyu_core::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_core_presets_and_types() {
        assert_eq!(env_profile::presets::PresetKind::MacOs.to_string(), "macos");
        assert_eq!(driver::EngineKind::Chromium.to_string(), "chromium");
    }
}
