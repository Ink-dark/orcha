use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `Artifact.type` 取值（README §5.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArtifactType {
    CodeDiff,
    Log,
    Report,
    FileRef,
}

/// Orcha 在执行过程中产出的不可变制品（README §5.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    /// 形如 `ART-001`。
    pub artifact_id: String,
    /// 产物类型。
    #[serde(rename = "type")]
    pub artifact_type: ArtifactType,
    /// 关联的 git commit sha（仅 `CodeDiff` 必填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// 可应用的 patch 文本（`CodeDiff` 时为 unified diff）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// 产物的可访问 URL 或本地路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
