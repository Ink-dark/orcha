// Orcha Core - Cycleround loop and task orchestration.
// See docs/ROADMAP.md M0/M1/M2/M3 for the contract this crate fulfills.

pub mod cycleround;
pub mod error;
pub mod history;
pub mod memory;
pub mod recovery;
pub mod sandbox;
pub mod state_machine;
pub mod store;
pub mod sub_agent;
pub mod sub_agents;

#[cfg(feature = "llm")]
pub mod llm_agents;

pub use cycleround::{CycleConfig, CycleOutcome, Cycleround, FailureReason, RoundRecord};
pub use error::CoreError;
pub use history::{FileHistoryStore, HistoryStore};
pub use memory::{FileMemoryStore, MemoryEntry, MemoryStore};
pub use recovery::{RecoverStrategy, Recovery, RecoveryReport};
pub use sandbox::{FsSandbox, Sandbox, Workspace};
pub use state_machine::{is_legal_transition, is_terminal, transition};
pub use store::{FileTaskStore, TaskStore};
pub use sub_agent::{mark_running, StepContext, StepOutput, SubAgent};
pub use sub_agents::{find_python, Fixer, Observer, Planner, Reviewer, Tester, Worker};

#[cfg(feature = "llm")]
pub use llm_agents::{LlmCycleround, LlmPlanner, LlmReviewer, LlmWorker};
