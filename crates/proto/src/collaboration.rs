//! Process-local parent/child collaboration state exposed by the Pi harness.
//!
//! None of these records enter the CRDT transcript. They are a live control
//! surface for the device hosting the Pi process and disappear with that run.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildAgentStatus {
    Starting,
    Working,
    NeedsAttention,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildAgent {
    /// Package-owned async run id. It is opaque to Nova, but is the exact
    /// control target accepted by pi-subagents.
    pub id: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub status: ChildAgentStatus,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollaborationSpeaker {
    You,
    Parent,
    Child,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationMessage {
    pub id: String,
    pub speaker: CollaborationSpeaker,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationSession {
    pub chat_id: String,
    /// True after the companion bridge has reached pi-subagents' v1 RPC.
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub parent_working: bool,
    #[serde(default)]
    pub children: Vec<ChildAgent>,
    #[serde(default)]
    pub messages: Vec<CollaborationMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationAction {
    Spawn,
    Steer,
    FollowUp,
    Resume,
    Stop,
    Interrupt,
    Room,
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationControlRequest {
    pub chat_id: String,
    pub action: CollaborationAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationControlReply {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
