//! orcha-sdk 模型层的序列化/反序列化与 JSON Schema 完整性测试。

use orcha_sdk::{
    schema_for_all, Artifact, ArtifactType, Attachment, EventPayload, EventType, OrchaEvent,
    OrchaResponse, ResponseArtifactType, ResponseStatus, Step, StepResult, StepStatus, Task,
    TaskStatus,
};
use serde_json::json;

#[test]
fn task_new_is_pending_with_t_prefix_id() {
    let id = Task::generate_id();
    assert!(id.starts_with("T-"), "id should start with T-: {id}");
    let task = Task::new(id.clone(), "fix auth".into());
    assert_eq!(task.id, id);
    assert_eq!(task.description, "fix auth");
    assert_eq!(task.status, TaskStatus::Pending);
    assert_eq!(task.created_at, task.updated_at);
}

#[test]
fn task_status_serializes_as_screaming_snake_case() {
    let task = Task::new("T-1".into(), "x".into());
    let v = serde_json::to_value(&task).unwrap();
    assert_eq!(v["status"], "PENDING");
}

#[test]
fn task_status_round_trips_through_json() {
    for &status in TaskStatus::all() {
        let task = Task {
            id: "T-1".into(),
            description: "x".into(),
            status,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 0,
        };
        let j = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&j).unwrap();
        assert_eq!(task, back, "round-trip failed for {status:?}");
    }
}

#[test]
fn task_status_display_matches_serde_repr() {
    for &status in TaskStatus::all() {
        assert_eq!(
            status.to_string(),
            serde_json::to_value(status).unwrap().as_str().unwrap()
        );
    }
}

#[test]
fn orcha_event_round_trip_matches_spec_shape() {
    let evt = OrchaEvent {
        event_id: "EVT-xxxx".into(),
        source: "slack".into(),
        user_id: "U123".into(),
        timestamp: 1_700_000_000,
        event_type: EventType::UserPrompt,
        payload: EventPayload {
            raw_text: "@Orcha fix the bug in auth.py".into(),
            attachments: vec![Attachment {
                mime_type: "text/plain".into(),
                url: "file:///tmp/log.txt".into(),
            }],
        },
    };
    let v = serde_json::to_value(&evt).unwrap();
    // README §3.1 的字段名/枚举形。
    assert_eq!(v["event_id"], "EVT-xxxx");
    assert_eq!(v["source"], "slack");
    assert_eq!(v["type"], "USER_PROMPT");
    assert_eq!(v["payload"]["raw_text"], "@Orcha fix the bug in auth.py");

    let back: OrchaEvent = serde_json::from_value(v).unwrap();
    assert_eq!(evt, back);
}

#[test]
fn orcha_response_default_payload_shape() {
    let resp = OrchaResponse::final_empty("EVT-1");
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["event_id"], "EVT-1");
    assert_eq!(v["status"], "FINAL");
    assert_eq!(v["content"], "");
    assert!(v["artifacts"].is_array());
    assert!(v["artifacts"].as_array().unwrap().is_empty());
}

#[test]
fn response_status_display() {
    assert_eq!(ResponseStatus::Streaming.to_string(), "STREAMING");
    assert_eq!(ResponseStatus::Final.to_string(), "FINAL");
    assert_eq!(ResponseStatus::Error.to_string(), "ERROR");
}

#[test]
fn step_result_duration_is_non_negative_when_finished_after_start() {
    let start = chrono::Utc::now();
    let end = start + chrono::Duration::seconds(5);
    let res = StepResult {
        step_id: "S-1".into(),
        success: true,
        started_at: start,
        finished_at: end,
        summary: "ok".into(),
        artifact_ids: vec![],
    };
    assert_eq!(res.duration().num_seconds(), 5);
}

#[test]
fn step_serializes_status_field() {
    let step = Step {
        id: "S-1".into(),
        name: "coder".into(),
        agent: "worker".into(),
        status: StepStatus::Running,
    };
    let v = serde_json::to_value(&step).unwrap();
    assert_eq!(v["status"], "RUNNING");
    assert_eq!(v["agent"], "worker");
}

#[test]
fn artifact_optional_fields_skip_when_none() {
    let art = Artifact {
        artifact_id: "ART-001".into(),
        artifact_type: ArtifactType::CodeDiff,
        commit_sha: None,
        patch: None,
        url: None,
    };
    let v = serde_json::to_value(&art).unwrap();
    assert_eq!(v["artifact_id"], "ART-001");
    assert_eq!(v["type"], "CODE_DIFF");
    assert!(v.get("commit_sha").is_none());
    assert!(v.get("patch").is_none());
    assert!(v.get("url").is_none());
}

#[test]
fn artifact_with_patch_round_trips() {
    let art = Artifact {
        artifact_id: "ART-002".into(),
        artifact_type: ArtifactType::CodeDiff,
        commit_sha: Some("abc123".into()),
        patch: Some("diff --git a/x b/x".into()),
        url: Some("file:///tmp/x.patch".into()),
    };
    let j = serde_json::to_string(&art).unwrap();
    let back: Artifact = serde_json::from_str(&j).unwrap();
    assert_eq!(art, back);
}

#[test]
fn schema_for_all_covers_every_model() {
    let schema = schema_for_all();
    let obj = schema.as_object().expect("schema must be a JSON object");
    for key in [
        "Task",
        "TaskStatus",
        "OrchaEvent",
        "EventType",
        "EventPayload",
        "Attachment",
        "OrchaResponse",
        "ResponseStatus",
        "ResponseArtifact",
        "ResponseArtifactType",
        "Step",
        "StepStatus",
        "StepResult",
        "Artifact",
        "ArtifactType",
    ] {
        assert!(obj.contains_key(key), "schema missing model: {key}");
        let entry = &obj[key];
        assert_eq!(
            entry["$schema"], "http://json-schema.org/draft-07/schema#",
            "model {key} missing $schema"
        );
    }
}

#[test]
fn schema_for_task_enum_matches_spec_values() {
    let schema = schema_for_all();
    let variants = schema["TaskStatus"]["enum"]
        .as_array()
        .expect("TaskStatus enum missing");
    let values: Vec<&str> = variants.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(
        values,
        vec!["PENDING", "RUNNING", "BLOCKED", "DONE", "FAILED"]
    );
}

#[test]
fn response_artifact_type_enum_serialization() {
    assert_eq!(
        serde_json::to_value(ResponseArtifactType::Diff).unwrap(),
        json!("DIFF")
    );
    assert_eq!(
        serde_json::to_value(ResponseArtifactType::Url).unwrap(),
        json!("URL")
    );
}
