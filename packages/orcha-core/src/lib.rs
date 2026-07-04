// Orcha Core - Cycleround loop and task orchestration.
// See docs/ROADMAP.md M0/M1 for the contract this crate fulfills.

pub mod error;
pub mod state_machine;
pub mod store;
pub mod sub_agent;

pub use error::CoreError;
pub use state_machine::{is_legal_transition, is_terminal, transition};
pub use store::{FileTaskStore, TaskStore};
pub use sub_agent::{mark_running, StepContext, StepOutput, SubAgent};
