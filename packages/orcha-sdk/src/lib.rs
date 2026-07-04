// Orcha SDK - shared data models and types.
// See docs/ROADMAP.md M0 for the contract this crate fulfills.

pub mod models;

pub use models::{
    artifact::{Artifact, ArtifactType},
    event::{Attachment, EventPayload, EventType, OrchaEvent},
    response::{OrchaResponse, ResponseArtifact, ResponseArtifactType, ResponseStatus},
    step::{Step, StepResult, StepStatus},
    task::{Task, TaskStatus},
};

/// 为所有模型一次性导出 JSON Schema。
///
/// 返回一个 JSON 对象，键为模型名，值为该模型的 JSON Schema。
/// `orcha schema --export` 直接序列化本函数返回值。
pub fn schema_for_all() -> serde_json::Value {
    serde_json::json!({
        "Task": schemars::schema_for!(Task),
        "TaskStatus": schemars::schema_for!(TaskStatus),
        "OrchaEvent": schemars::schema_for!(OrchaEvent),
        "EventType": schemars::schema_for!(EventType),
        "EventPayload": schemars::schema_for!(EventPayload),
        "Attachment": schemars::schema_for!(Attachment),
        "OrchaResponse": schemars::schema_for!(OrchaResponse),
        "ResponseStatus": schemars::schema_for!(ResponseStatus),
        "ResponseArtifact": schemars::schema_for!(ResponseArtifact),
        "ResponseArtifactType": schemars::schema_for!(ResponseArtifactType),
        "Step": schemars::schema_for!(Step),
        "StepStatus": schemars::schema_for!(StepStatus),
        "StepResult": schemars::schema_for!(StepResult),
        "Artifact": schemars::schema_for!(Artifact),
        "ArtifactType": schemars::schema_for!(ArtifactType),
    })
}
