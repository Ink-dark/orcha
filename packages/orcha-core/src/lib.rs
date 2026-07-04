// Orcha Core - Cycleround loop and task orchestration.
// See docs/ROADMAP.md M0 for the contract this crate fulfills.

pub mod error;
pub mod state_machine;

pub use error::CoreError;
pub use state_machine::{is_legal_transition, is_terminal, transition};
