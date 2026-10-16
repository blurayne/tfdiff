use crate::common::test_data::*;
use tfdiff::formatter::html::*;
use tfdiff::models::*;

#[cfg(test)]
mod html_formatter_tests {
    use super::*;

    #[test]
    fn changed_resource_identity_uses_priority_escapes_html_and_can_be_suppressed() {
        let mut resource = sample_update_resource();
        resource.attributes = [
            ("id".to_string(), serde_json::json!("id-<script>")),
            ("arn".to_string(), serde_json::json!("arn-123")),
            ("name".to_string(), serde_json::json!("name-123")),
        ]
        .into_iter()
        .collect();
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let output = format_html_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(output.contains("id: id-&lt;script&gt;"));
        assert!(!output.contains("arn: arn-123"));
        assert!(!output.contains("name: name-123"));

        let output = format_html_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: false,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(!output.contains("id: id-&lt;script&gt;"));
        assert!(!output.contains("id-<script>"));
    }

    #[test]
    fn test_format_html_output_basic() {
        let plan = sample_terraform_plan();
        let output = format_html_output(&plan);

        assert!(output.contains("<!DOCTYPE html>"));
        assert!(output.contains("<html lang=\"en\">"));
        assert!(output.contains("Terraform Plan Analysis"));
        assert!(output.contains("Plan Summary"));
        assert!(output.contains(">2<"));
        assert!(output.contains("to add"));
        assert!(output.contains(">1<"));
        assert!(output.contains("to change"));
    }

    #[test]
    fn test_format_html_output_apply_mode() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::Apply;

        let output = format_html_output(&plan);
        assert!(output.contains("Terraform Plan Analysis"));
    }

    #[test]
    fn renders_source_versions_and_report_diffs() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::PlanDiff;
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

        let output = format_html_output(&plan);
        assert!(output.contains("Previous Version"));
        assert!(output.contains("older"));
        assert!(output.contains("2026-01-01T00:00:00Z"));
        assert!(output.contains("Selected Version"));
        assert!(output.contains("newer"));
        assert!(output.contains("2026-01-02T00:00:00Z"));
        assert!(output.contains("Earlier Plan Report"));
        assert!(output.contains("Later Plan Report"));
    }

    #[test]
    fn test_format_html_output_empty_plan() {
        let plan = TerraformPlan {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary::default(),
            resources: Vec::new(),
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        };

        let output = format_html_output(&plan);
        assert!(output.contains("<!DOCTYPE html>"));
        assert!(output.contains("Terraform Plan Analysis"));
        // Should not contain summary items when all counts are 0
        assert!(!output.contains("to add"));
        assert!(!output.contains("to change"));
        assert!(!output.contains("to destroy"));
    }

    #[test]
    fn test_format_html_output_with_destroy() {
        let plan = TerraformPlan {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary {
                add: 1,
                change: 0,
                destroy: 2,
                read: 0,
            },
            resources: vec![sample_create_resource(), sample_destroy_resource()],
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        };

        let output = format_html_output(&plan);
        assert!(output.contains(">1<"));
        assert!(output.contains("to add"));
        assert!(output.contains(">2<"));
        assert!(output.contains("to destroy"));
        assert!(output.contains("aws_s3_bucket.test"));
        assert!(output.contains("aws_instance.legacy"));
    }

    #[test]
    fn test_format_html_output_with_read_resources() {
        let plan = TerraformPlan {
            mode: PlanMode::Plan,
            source_versions: None,
            author_attribution: None,
            compact_sources: None,
            summary: Summary {
                add: 0,
                change: 0,
                destroy: 0,
                read: 3,
            },
            resources: Vec::new(),
            data_sources: Vec::new(),
            warnings: Vec::new(),
            metadata: Metadata::default(),
        };

        let output = format_html_output(&plan);
        assert!(output.contains(">3<"));
        assert!(output.contains("to read"));
    }

    #[test]
    fn test_format_html_output_uses_simple_semantic_change_table() {
        let plan = sample_terraform_plan();
        let output = format_html_output(&plan);

        assert!(output.contains("<table>"));
        assert!(output.contains("scope=\"col\">Attribute</th>"));
        assert!(output.contains("scope=\"col\">Current State</th>"));
        assert!(output.contains("scope=\"col\">After Apply</th>"));
        assert!(output.contains("class=\"value before\""));
        assert!(output.contains("class=\"value after\""));
        assert!(output.contains("action-create"));
        assert!(output.contains("action-update"));
        assert!(!output.contains("linear-gradient"));
        assert!(!output.contains("@keyframes"));
        assert!(!output.contains("<script"));
    }

    #[test]
    fn test_format_html_output_is_responsive_and_accessible() {
        let plan = sample_terraform_plan();
        let output = format_html_output(&plan);

        assert!(output.contains("@media (max-width: 768px)"));
        assert!(output.contains("name=\"viewport\""));
        assert!(output.contains("scope=\"col\""));
        assert!(output.contains("lang=\"en\""));
        assert!(output.contains("aria-label=\"Plan Summary\""));
    }

    #[test]
    fn test_format_html_output_resources_with_attributes() {
        let plan = sample_terraform_plan();
        let output = format_html_output(&plan);

        assert!(output.contains("bucket"));
        assert!(output.contains("test-bucket"));
    }

    #[test]
    fn test_update_changes_remain_paired_when_values_are_added_and_removed() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::StateDiff;
        plan.resources = vec![Resource {
            id: "test_resource.item".to_string(),
            plan_delta: None,
            name: "item".to_string(),
            type_name: "test_resource".to_string(),
            provider: "test".to_string(),
            action: ActionType::Update,
            changes: vec![
                Change {
                    path: vec!["removed".to_string()],
                    before: Some(serde_json::json!("OLD_ONLY")),
                    after: None,
                    sensitive: false,
                    computed: false,
                },
                Change {
                    path: vec!["changed".to_string()],
                    before: Some(serde_json::json!("BEFORE")),
                    after: Some(serde_json::json!("AFTER")),
                    sensitive: false,
                    computed: false,
                },
                Change {
                    path: vec!["added".to_string()],
                    before: None,
                    after: Some(serde_json::json!("NEW_ONLY")),
                    sensitive: false,
                    computed: false,
                },
                Change {
                    path: vec!["computed".to_string()],
                    before: None,
                    after: Some(serde_json::json!("(known after apply)")),
                    sensitive: false,
                    computed: true,
                },
            ],
            attributes: Default::default(),
            audit_attribution: None,
            applied: false,
            source: None,
        }];

        let output = format_html_output(&plan);
        let removed_row = output
            .split("<tr class=\"row-remove\">")
            .nth(1)
            .unwrap()
            .split("</tr>")
            .next()
            .unwrap();
        assert!(removed_row.contains("removed"));
        assert!(removed_row.contains("OLD_ONLY"));
        assert!(removed_row.contains("aria-label=\"No value\""));

        let changed_row = output
            .split("<tr class=\"row-change\">")
            .nth(1)
            .unwrap()
            .split("</tr>")
            .next()
            .unwrap();
        assert!(changed_row.contains("changed"));
        assert!(changed_row.contains("BEFORE"));
        assert!(changed_row.contains("AFTER"));

        let added_row = output
            .split("<tr class=\"row-add\">")
            .nth(1)
            .unwrap()
            .split("</tr>")
            .next()
            .unwrap();
        assert!(added_row.contains("NEW_ONLY"));
        assert!(added_row.contains("aria-label=\"No value\""));

        let computed_row = output
            .split("<tr class=\"row-add\">")
            .nth(2)
            .unwrap()
            .split("</tr>")
            .next()
            .unwrap();
        assert!(computed_row.contains("computed"));
        assert!(computed_row.contains("known after apply"));
    }

    #[test]
    fn test_text_plan_fallback_diff_notation_is_paired() {
        let plan = TerraformPlan {
            resources: vec![Resource {
                id: "test_resource.item".to_string(),
                plan_delta: None,
                name: "item".to_string(),
                type_name: "test_resource".to_string(),
                provider: "test".to_string(),
                action: ActionType::Update,
                changes: vec![],
                attributes: [
                    ("removed".to_string(), serde_json::json!("- old")),
                    ("changed".to_string(), serde_json::json!("before → after")),
                    ("added".to_string(), serde_json::json!("+ new")),
                ]
                .into_iter()
                .collect(),
                audit_attribution: None,
                applied: false,
                source: None,
            }],
            ..Default::default()
        };
        let output = format_html_output(&plan);

        let remove_row = output.split("<tr class=\"row-remove\">").nth(1).unwrap();
        assert!(remove_row.split("</tr>").next().unwrap().contains("old"));
        let change_row = output.split("<tr class=\"row-change\">").nth(1).unwrap();
        let change_row = change_row.split("</tr>").next().unwrap();
        assert!(change_row.contains("before"));
        assert!(change_row.contains("after"));
        let add_row = output.split("<tr class=\"row-add\">").nth(1).unwrap();
        assert!(add_row.split("</tr>").next().unwrap().contains("new"));
    }

    #[test]
    fn empty_objects_render_with_one_pair_of_braces() {
        let plan = TerraformPlan {
            resources: vec![Resource {
                id: "test_resource.item".to_string(),
                plan_delta: None,
                name: "item".to_string(),
                type_name: "test_resource".to_string(),
                provider: "test".to_string(),
                action: ActionType::Create,
                changes: vec![],
                attributes: [("empty".to_string(), serde_json::json!({}))]
                    .into_iter()
                    .collect(),
                audit_attribution: None,
                applied: false,
                source: None,
            }],
            ..Default::default()
        };

        assert!(format_html_output(&plan).contains("{}"));
        assert!(!format_html_output(&plan).contains("{{}}"));
    }
}
