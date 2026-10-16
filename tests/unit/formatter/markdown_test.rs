use crate::common::test_data::*;
use tfdiff::formatter::markdown::*;
use tfdiff::models::*;

#[cfg(test)]
mod markdown_formatter_tests {
    use super::*;

    #[test]
    fn changed_resource_identity_is_formatted_safely_and_can_be_suppressed() {
        let mut resource = sample_update_resource();
        resource.attributes = [
            ("arn".to_string(), serde_json::json!("arn-123")),
            ("id".to_string(), serde_json::json!("id-`<one>\\nnext")),
        ]
        .into_iter()
        .collect();
        let plan = TerraformPlan {
            resources: vec![resource.clone()],
            ..Default::default()
        };
        let output = format_markdown_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(output.contains("**id:** ``id-`<one>\\nnext``"));

        resource
            .attributes
            .insert("id".to_string(), serde_json::json!(" padded "));
        let padded_plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let padded_output = format_markdown_output_with_options(
            &padded_plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(padded_output.contains("**id:** `  padded  `"));
        assert!(!output.contains("**arn:**"));

        let output = format_markdown_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: false,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(!output.contains("**id:**"));
        assert!(!output.contains("id-`<one>\\nnext"));
    }

    #[test]
    fn test_format_markdown_output_basic() {
        let plan = sample_terraform_plan();
        let output = format_markdown_output(&plan);

        assert!(output.contains("# Terraform Plan Report"));
        assert!(output.contains("## Summary"));
        assert!(output.contains("- ✅ **2** resources to add"));
        assert!(output.contains("- 🔄 **1** resources to change"));
        assert!(output.contains("- ❌ **0** resources to destroy"));
    }

    #[test]
    fn test_format_markdown_output_apply_mode() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::Apply;

        let output = format_markdown_output(&plan);
        assert!(output.contains("# Terraform Apply Report"));
    }

    #[test]
    fn test_format_markdown_output_with_resources() {
        let plan = sample_terraform_plan();
        let output = format_markdown_output(&plan);

        assert!(output.contains("## Resource Changes"));
        assert!(output.contains("### ✅ CREATE `aws_s3_bucket.test`"));
        assert!(output.contains("### 🔄 UPDATE `aws_instance.web`"));
        assert!(output.contains("```json"));
    }

    #[test]
    fn test_format_markdown_output_with_read_resources() {
        let plan = TerraformPlan {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary {
                add: 0,
                change: 0,
                destroy: 0,
                read: 2,
            },
            resources: Vec::new(),
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        };

        let output = format_markdown_output(&plan);
        assert!(output.contains("- 📖 **2** resources to read"));
    }

    #[test]
    fn test_format_markdown_output_with_destroy() {
        let plan = TerraformPlan {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary {
                add: 0,
                change: 0,
                destroy: 1,
                read: 0,
            },
            resources: vec![sample_destroy_resource()],
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        };

        let output = format_markdown_output(&plan);
        assert!(output.contains("- ❌ **1** resources to destroy"));
        assert!(output.contains("### ❌ DESTROY `aws_instance.legacy`"));
    }

    #[test]
    fn test_format_markdown_output_with_warnings() {
        let mut plan = sample_terraform_plan();
        plan.warnings = vec![
            Warning {
                message: "This is a test warning".to_string(),
                level: WarningLevel::Warning,
            },
            Warning {
                message: "This is an error".to_string(),
                level: WarningLevel::Error,
            },
            Warning {
                message: "This is info".to_string(),
                level: WarningLevel::Info,
            },
        ];

        let output = format_markdown_output(&plan);
        assert!(output.contains("## Warnings"));
        assert!(output.contains("- ⚠️ This is a test warning"));
        assert!(output.contains("- 🚨 This is an error"));
        assert!(output.contains("- ℹ️ This is info"));
    }

    #[test]
    fn test_format_markdown_output_with_metadata() {
        let plan = sample_terraform_plan();
        let output = format_markdown_output(&plan);

        assert!(output.contains("## Metadata"));
        assert!(output.contains("- **Terraform Version:** 1.5.7"));
        assert!(output.contains("- **Generated:** 2024-01-01T12:00:00Z"));
        assert!(output.contains("- **Duration:** 5s"));
    }

    #[test]
    fn formats_source_version_ids_and_timestamps() {
        let mut plan = sample_terraform_plan();
        plan.source_versions = Some(SourceVersions {
            left_label: "Previous Version".to_string(),
            right_label: "Selected Version".to_string(),
            left: Some(SourceVersion {
                version_id: Some("older".to_string()),
                last_modified: Some("2026-01-01T00:00:00Z".to_string()),
                etag: None,
            }),
            right: Some(SourceVersion {
                version_id: Some("newer".to_string()),
                last_modified: Some("2026-01-02T00:00:00Z".to_string()),
                etag: None,
            }),
        });

        let output = format_markdown_output(&plan);
        assert!(output.contains("Previous Version"));
        assert!(output.contains("`older` — 2026-01-01T00:00:00Z"));
        assert!(output.contains("`newer` — 2026-01-02T00:00:00Z"));
    }

    #[test]
    fn test_format_markdown_output_empty_plan() {
        let plan = TerraformPlan::default();
        let output = format_markdown_output(&plan);

        assert!(output.contains("# Terraform Plan Report"));
        assert!(output.contains("## Summary"));
        assert!(output.contains("- ✅ **0** resources to add"));
        assert!(!output.contains("## Resource Changes"));
        assert!(!output.contains("## Warnings"));
        assert!(!output.contains("## Metadata"));
    }

    #[test]
    fn test_format_markdown_output_action_emojis() {
        let plan = TerraformPlan {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary::default(),
            resources: vec![
                Resource {
                    id: "test.create".to_string(),
                    plan_delta: None,
                    name: "create".to_string(),
                    type_name: "test".to_string(),
                    provider: "test".to_string(),
                    action: ActionType::Create,
                    changes: Vec::new(),
                    attributes: std::collections::HashMap::new(),
                    audit_attribution: None,
                    applied: false,
                    source: None,
                },
                Resource {
                    id: "test.update".to_string(),
                    plan_delta: None,
                    name: "update".to_string(),
                    type_name: "test".to_string(),
                    provider: "test".to_string(),
                    action: ActionType::Update,
                    changes: Vec::new(),
                    attributes: std::collections::HashMap::new(),
                    audit_attribution: None,
                    applied: false,
                    source: None,
                },
                Resource {
                    id: "test.destroy".to_string(),
                    plan_delta: None,
                    name: "destroy".to_string(),
                    type_name: "test".to_string(),
                    provider: "test".to_string(),
                    action: ActionType::Destroy,
                    changes: Vec::new(),
                    attributes: std::collections::HashMap::new(),
                    audit_attribution: None,
                    applied: false,
                    source: None,
                },
                Resource {
                    id: "test.read".to_string(),
                    plan_delta: None,
                    name: "read".to_string(),
                    type_name: "test".to_string(),
                    provider: "test".to_string(),
                    action: ActionType::Read,
                    changes: Vec::new(),
                    attributes: std::collections::HashMap::new(),
                    audit_attribution: None,
                    applied: false,
                    source: None,
                },
                Resource {
                    id: "test.noop".to_string(),
                    plan_delta: None,
                    name: "noop".to_string(),
                    type_name: "test".to_string(),
                    provider: "test".to_string(),
                    action: ActionType::NoOp,
                    changes: Vec::new(),
                    attributes: std::collections::HashMap::new(),
                    audit_attribution: None,
                    applied: false,
                    source: None,
                },
            ],
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        };

        let output = format_markdown_output(&plan);
        assert!(output.contains("### ✅ CREATE"));
        assert!(output.contains("### 🔄 UPDATE"));
        assert!(output.contains("### ❌ DESTROY"));
        assert!(output.contains("### 📖 READ"));
        assert!(output.contains("### ⭕ NO-OP"));
    }

    #[test]
    fn test_format_markdown_output_footer() {
        let plan = sample_terraform_plan();
        let output = format_markdown_output(&plan);

        assert!(output.contains("---"));
        assert!(output.contains("*Generated by tfdiff*"));
    }
}
