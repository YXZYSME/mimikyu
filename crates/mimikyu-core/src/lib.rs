pub mod discovery;
pub mod driver;
pub mod error;
pub mod process;

// M2 modules (stubs in M1)
pub mod env_profile;
pub mod isolation;
pub mod network;

// M3 modules (stubs in M1)
pub mod extract;
pub mod input;
pub mod snapshot;
pub mod wait;

// M4 modules (stubs in M1)
pub mod bench;
pub mod daemon;
pub mod harness;
pub mod mcp;
pub mod session;

pub use error::{MimikyuError, Result};
