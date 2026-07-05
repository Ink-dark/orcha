// Orcha Shell - gateway adapters for IM and HTTP.
// See docs/ROADMAP.md M5 for the contract this crate fulfills.

pub mod adapter;
pub mod cli_adapter;
pub mod error;
pub mod shell;

pub use adapter::{AdapterInfo, ShellAdapter};
pub use cli_adapter::CliAdapter;
pub use error::{Result, ShellError};
pub use shell::OrchaShell;
