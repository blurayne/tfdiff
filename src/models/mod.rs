use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerraformPlan {
    pub mode: PlanMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_versions: Option<SourceVersions>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub author_attribution: Option<crate::authors::AuditReport>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub compact_sources: Option<CompactSources>,
    pub summary: Summary,
    pub resources: Vec<Resource>,
    pub data_sources: Vec<DataSource>,
    pub warnings: Vec<Warning>,
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceVersions {
    pub left_label: String,
    pub right_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<SourceVersion>,
    pub right: Option<SourceVersion>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactSources {
    pub create: Vec<SourceRef>,
    pub update: Vec<SourceRef>,
    pub destroy: Vec<SourceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRef {
    pub key: String,
    pub from_version: Option<String>,
    pub until_version: Option<String>,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceVersion {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub etag: Option<String>,
}

impl Default for TerraformPlan {
    fn default() -> Self {
        Self {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary::default(),
            resources: Vec::new(),
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlanMode {
    Plan,
    Apply,
    StateDiff,
    PlanDiff,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Summary {
    pub add: usize,
    pub change: usize,
    pub destroy: usize,
    pub read: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_delta: Option<PlanDelta>,
    pub name: String,
    pub type_name: String,
    pub provider: String,
    pub action: ActionType,
    pub changes: Vec<Change>,
    pub attributes: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub audit_attribution: Option<ResourceAuditAttribution>,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceAuditAttribution {
    pub status: crate::authors::AuditStatus,
    #[serde(default)]
    pub observations: Vec<crate::authors::AuditObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDelta {
    Added,
    Changed,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSource {
    pub id: String,
    pub name: String,
    pub type_name: String,
    pub provider: String,
    pub attributes: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ActionType {
    Create,
    Update,
    Destroy,
    Read,
    NoOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub path: Vec<String>,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub sensitive: bool,
    pub computed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub message: String,
    pub level: WarningLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WarningLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metadata {
    pub terraform_version: Option<String>,
    pub timestamp: Option<String>,
    pub duration: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_json_omits_compact_fields() {
        let plan = serde_json::to_value(TerraformPlan::default()).unwrap();
        assert!(plan.get("compact_sources").is_none());

        let resource = Resource {
            id: "aws_s3_bucket.a".to_string(),
            plan_delta: None,
            name: "a".to_string(),
            type_name: "aws_s3_bucket".to_string(),
            provider: "aws".to_string(),
            action: ActionType::Create,
            changes: Vec::new(),
            attributes: HashMap::new(),
            audit_attribution: None,
            applied: false,
            source: None,
        };
        let value = serde_json::to_value(&resource).unwrap();
        assert!(value.get("source").is_none());
    }
}
