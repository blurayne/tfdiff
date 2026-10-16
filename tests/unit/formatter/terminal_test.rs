use crate::common::test_data::*;
use tfdiff::formatter::terminal::*;
use tfdiff::models::*;
use tfdiff::parser::cleaner::clean_ansi_codes;

#[cfg(test)]
mod terminal_formatter_tests {
    use super::*;

    #[test]
    fn changed_resource_identity_uses_id_then_arn_then_name_and_can_be_suppressed() {
        let mut resource = sample_update_resource();
        resource.attributes = [
            ("id".to_string(), serde_json::json!("resource-id")),
            ("arn".to_string(), serde_json::json!("resource-arn")),
            ("name".to_string(), serde_json::json!("resource-name")),
        ]
        .into_iter()
        .collect();
        let plan = TerraformPlan {
            resources: vec![resource.clone()],
            ..Default::default()
        };
        let output = format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(clean_ansi_codes(&output).contains("id: resource-id"));
        assert!(!clean_ansi_codes(&output).contains("arn: resource-arn"));
        assert!(!clean_ansi_codes(&output).contains("name: resource-name"));

        let output = format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: false,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(!clean_ansi_codes(&output).contains("id: resource-id"));

        resource.attributes.remove("id");
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let output = format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(clean_ansi_codes(&output).contains("arn: resource-arn"));
        assert!(!clean_ansi_codes(&output).contains("name: resource-name"));
    }

    #[test]
    fn destroy_identity_uses_before_value_and_never_exposes_sensitive_identity() {
        let mut resource = sample_destroy_resource();
        resource.attributes.clear();
        resource.changes = vec![
            Change {
                path: vec!["id".into()],
                before: Some(serde_json::json!("destroyed-id")),
                after: None,
                sensitive: false,
                computed: false,
            },
            Change {
                path: vec!["arn".into()],
                before: Some(serde_json::json!("secret-arn")),
                after: None,
                sensitive: true,
                computed: false,
            },
        ];
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let output = format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        );
        assert!(output.contains("id: destroyed-id"));
        assert!(!output.contains("secret-arn"));
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

        let output = format_terminal_output(&plan);
        assert!(output.contains("Previous Version"));
        assert!(output.contains("older"));
        assert!(output.contains("2026-01-01T00:00:00Z"));
        assert!(output.contains("Selected Version"));
        assert!(output.contains("newer"));
        assert!(output.contains("2026-01-02T00:00:00Z"));
    }

    #[test]
    fn s3_provenance_is_compact_sanitized_and_color_aware() {
        let mut plan = sample_terraform_plan();
        plan.source_versions = Some(SourceVersions {
            left_label: "Previous Version".to_string(),
            right_label: "Selected Version".to_string(),
            left: Some(SourceVersion {
                version_id: Some("old\u{1b}[31m-version".to_string()),
                last_modified: Some("2026-01-01T00:00:00Z".to_string()),
                etag: None,
            }),
            right: Some(SourceVersion {
                version_id: Some("new-version".to_string()),
                last_modified: Some("2026-01-02T00:00:00Z".to_string()),
                etag: None,
            }),
        });
        let uri = "s3://bucket/state\nunsafe.tfstate";
        let output = format_terminal_output_with_s3_provenance(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
            Some(uri),
        );

        assert!(output.contains("s3://bucket/stateunsafe.tfstate"));
        assert!(output.contains("- old[31m-version  (2026-01-01T00:00:00Z)"));
        assert!(output.contains("+ new-version  (2026-01-02T00:00:00Z)"));
        assert!(!output.contains("Previous Version"));
        assert!(!output.contains("Selected Version"));
        assert!(!output.contains("old\u{1b}[31m-version"));

        let missing_metadata = TerraformPlan {
            source_versions: Some(SourceVersions {
                left_label: "left".to_string(),
                right_label: "right".to_string(),
                left: Some(SourceVersion {
                    version_id: None,
                    last_modified: None,
                    etag: None,
                }),
                right: None,
            }),
            ..Default::default()
        };
        let output = format_terminal_output_with_s3_provenance(
            &missing_metadata,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
            Some("s3://bucket/state.tfstate"),
        );
        assert!(output.contains("- unknown  "));
        assert!(!output.contains("+ unknown"));
    }

    #[test]
    fn s3_state_diff_is_compact_unless_pretty_is_requested() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::StateDiff;
        let options = tfdiff::formatter::FormatterOptions {
            show_resource_identity: true,
            show_full_attribution: false,
            color: false,
        };
        let uri = "s3://bucket/state.tfstate";

        let compact = clean_ansi_codes(&format_terminal_output_with_s3_provenance_pretty(
            &plan,
            options,
            Some(uri),
            false,
            false,
        ));
        assert!(compact.starts_with(&format!("{}\n{uri}\n", "━".repeat(80))));
        assert!(!compact.contains("OBSERVED SNAPSHOT DIFFERENCES"));
        assert!(!compact.contains("RESOURCES"));
        assert!(!compact.contains("══"));
        assert!(!compact.contains("──"));

        let pretty = clean_ansi_codes(&format_terminal_output_with_s3_provenance_pretty(
            &plan,
            options,
            Some(uri),
            true,
            false,
        ));
        assert!(pretty.contains("RESOURCES"));
    }

    #[test]
    fn s3_resource_headers_show_state_date_for_create_update_and_destroy() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::StateDiff;
        plan.source_versions = Some(SourceVersions {
            left_label: "left".to_string(),
            right_label: "right".to_string(),
            left: Some(SourceVersion {
                version_id: Some("old".to_string()),
                last_modified: Some("2026-05-01T00:00:00Z".to_string()),
                etag: None,
            }),
            right: Some(SourceVersion {
                version_id: Some("new".to_string()),
                last_modified: Some("2026-05-29T13:12:33Z".to_string()),
                etag: None,
            }),
        });
        plan.resources = vec![sample_update_resource(), sample_destroy_resource()];
        let uri = "s3://bucket/state.tfstate";
        let options = tfdiff::formatter::FormatterOptions {
            show_resource_identity: true,
            show_full_attribution: false,
            color: false,
        };
        let full = clean_ansi_codes(&format_terminal_output_with_s3_provenance_pretty(
            &plan,
            options,
            Some(uri),
            false,
            true,
        ));
        assert!(full.contains("UPDATE  aws_instance.web  2026-05-29T13:12:33Z"));
        assert!(full.contains("DESTROY  aws_instance.legacy  2026-05-01T00:00:00Z"));

        let relative = clean_ansi_codes(&format_terminal_output_with_s3_provenance(
            &plan,
            options,
            Some(uri),
        ));
        let update_header = relative
            .lines()
            .find(|line| line.trim_start().starts_with("UPDATE"))
            .unwrap();
        let destroy_header = relative
            .lines()
            .find(|line| line.trim_start().starts_with("DESTROY"))
            .unwrap();
        assert!(update_header.ends_with(" ago"), "{update_header}");
        assert!(destroy_header.ends_with(" ago"), "{destroy_header}");
        assert!(!update_header.contains("2026-05-29T13:12:33Z"));
        assert!(!destroy_header.contains("2026-05-01T00:00:00Z"));
    }

    #[test]
    fn local_state_diff_keeps_full_layout() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::StateDiff;
        let output = clean_ansi_codes(&format_terminal_output(&plan));
        assert!(output.contains("RESOURCES"));
    }

    #[test]
    fn s3_uri_uses_bold_bright_white_when_color_is_enabled() {
        let plan = TerraformPlan::default();
        let output = format_terminal_output_with_s3_provenance(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: true,
            },
            Some("s3://bucket/state.tfstate"),
        );

        assert!(output.contains("\u{1b}[1;97ms3://bucket/state.tfstate\u{1b}[0m"));
    }

    fn attributed_create_resource() -> Resource {
        let mut resource = sample_create_resource();
        resource.audit_attribution = Some(ResourceAuditAttribution {
            status: tfdiff::authors::AuditStatus::Complete,
            observations: vec![tfdiff::authors::AuditObservation {
                event_id: "event-1".to_string(),
                event_time: "2026-10-13T10:00:00Z".to_string(),
                event_name: "CreateRole".to_string(),
                event_source: "iam.amazonaws.com".to_string(),
                account_id: Some("111122223333".to_string()),
                region: Some("us-east-1".to_string()),
                principal_type: Some("AssumedRole".to_string()),
                principal_arn: Some(
                    "arn:aws:sts::111122223333:assumed-role/admin/session".to_string(),
                ),
                principal_id: Some("AROAEXAMPLE:session".to_string()),
                username: None,
                source_identity: None,
                session_label: None,
                email_hint: Some("jane.doe@acme.example".to_string()),
                identity_hint_origin: Some("source_identity".to_string()),
                user_agent: None,
                client_hint: None,
                match_basis: "resource_id".to_string(),
            }],
        });
        resource
    }

    fn attribution_options(show_full_attribution: bool) -> tfdiff::formatter::FormatterOptions {
        tfdiff::formatter::FormatterOptions {
            show_resource_identity: true,
            show_full_attribution,
            color: false,
        }
    }

    #[test]
    fn resource_header_shows_principal_email_and_hides_detail_by_default() {
        let plan = TerraformPlan {
            resources: vec![attributed_create_resource()],
            ..Default::default()
        };

        let output = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            attribution_options(false),
        ));
        let header = output
            .lines()
            .find(|line| line.contains("CREATE") && line.contains("aws_s3_bucket.test"))
            .unwrap();
        assert!(header.ends_with("@jane.doe@acme.example"), "{header}");
        assert!(!output.contains("Audit attribution:"));
        assert!(!output.contains("Principal:"));
        assert!(!output.contains("arn:aws:sts"));
    }

    #[test]
    fn resource_detail_attribution_is_shown_with_full_attribution() {
        let plan = TerraformPlan {
            resources: vec![attributed_create_resource()],
            ..Default::default()
        };

        let output = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            attribution_options(true),
        ));
        assert!(output.contains("Audit attribution:"));
        assert!(output.contains("Principal: arn:aws:sts::111122223333:assumed-role/admin/session"));
    }

    #[test]
    fn header_has_no_principal_suffix_without_email_hint() {
        let mut resource = attributed_create_resource();
        resource.audit_attribution.as_mut().unwrap().observations[0].email_hint = None;
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };

        let output = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            attribution_options(false),
        ));
        assert!(!output.contains('@'));
    }

    #[test]
    fn report_attribution_block_is_gated_by_full_attribution() {
        let plan = TerraformPlan {
            author_attribution: Some(tfdiff::authors::AuditReport {
                window_start: "2026-10-13T00:00:00Z".to_string(),
                window_end: "2026-10-13T12:00:00Z".to_string(),
                regions: Vec::new(),
                show_atomic_authors: false,
                status: tfdiff::authors::AuditStatus::Complete,
                message: None,
                observations: Vec::new(),
            }),
            ..Default::default()
        };

        let hidden = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            attribution_options(false),
        ));
        assert!(!hidden.contains("Audit window:"));

        let shown = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            attribution_options(true),
        ));
        assert!(shown.contains("Audit window: 2026-10-13T00:00:00Z"));
    }

    #[test]
    fn test_format_terminal_output_basic() {
        let plan = sample_terraform_plan();
        let output = format_terminal_output(&plan);

        assert!(!clean_ansi_codes(&output).contains("TERRAFORM PLAN ANALYSIS"));
        assert!(clean_ansi_codes(&output).contains("2 resources to add"));
        assert!(clean_ansi_codes(&output).contains("1 resources to change"));
        assert!(clean_ansi_codes(&output).contains("aws_s3_bucket.test"));
        assert!(clean_ansi_codes(&output).contains("aws_instance.web"));
    }

    #[test]
    fn test_format_terminal_output_apply_mode() {
        let mut plan = sample_terraform_plan();
        plan.mode = PlanMode::Apply;

        let output = format_terminal_output(&plan);
        assert!(!clean_ansi_codes(&output).contains("TERRAFORM APPLY ANALYSIS"));
        let pretty = tfdiff::formatter::terminal::format_terminal_output_with_s3_provenance_pretty(
            &plan,
            tfdiff::formatter::FormatterOptions::default(),
            None,
            true,
            false,
        );
        assert!(clean_ansi_codes(&pretty).contains("TERRAFORM APPLY ANALYSIS"));
        assert!(pretty.contains("╔"));
    }

    #[test]
    fn test_format_terminal_output_empty_plan() {
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

        let output = format_terminal_output(&plan);
        assert!(!clean_ansi_codes(&output).contains("TERRAFORM PLAN ANALYSIS"));
        // Should not contain summary box when all counts are 0
        assert!(!output.contains("┌────────────────────────────────┐"));
    }

    #[test]
    fn test_format_terminal_output_with_destroy() {
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

        let output = format_terminal_output(&plan);
        assert!(clean_ansi_codes(&output).contains("1 resources to add"));
        assert!(clean_ansi_codes(&output).contains("2 resources to destroy"));
        assert!(clean_ansi_codes(&output).contains("CREATE"));
        assert!(clean_ansi_codes(&output).contains("DESTROY"));
    }

    #[test]
    fn test_format_terminal_output_with_read() {
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

        let output = format_terminal_output(&plan);
        assert!(clean_ansi_codes(&output).contains("3 data sources to read"));
    }

    #[test]
    fn test_format_resource_create() {
        let resource = sample_create_resource();
        let formatted = format_resource(&resource);

        assert!(clean_ansi_codes(&formatted).contains("CREATE"));
        assert!(clean_ansi_codes(&formatted).contains("aws_s3_bucket.test"));
        assert!(clean_ansi_codes(&formatted).contains("bucket = \"test-bucket\""));
        assert!(clean_ansi_codes(&formatted).contains("force_destroy = false"));
    }

    #[test]
    fn test_format_resource_update() {
        let resource = sample_update_resource();
        let formatted = format_resource(&resource);

        assert!(clean_ansi_codes(&formatted).contains("UPDATE"));
        assert!(clean_ansi_codes(&formatted).contains("aws_instance.web"));
        // The new format shows changes in diff format
        assert!(clean_ansi_codes(&formatted).contains("instance_type"));
        assert!(clean_ansi_codes(&formatted).contains("t3.small"));
    }

    #[test]
    fn test_format_resource_destroy() {
        let resource = sample_destroy_resource();
        let formatted = format_resource(&resource);

        assert!(clean_ansi_codes(&formatted).contains("DESTROY"));
        assert!(clean_ansi_codes(&formatted).contains("aws_instance.legacy"));
    }

    #[test]
    fn test_format_resource_no_op() {
        let mut resource = sample_create_resource();
        resource.action = ActionType::NoOp;

        let formatted = format_resource(&resource);
        assert!(clean_ansi_codes(&formatted).contains("NO-OP"));
    }

    #[test]
    fn test_format_resource_read() {
        let mut resource = sample_create_resource();
        resource.action = ActionType::Read;

        let formatted = format_resource(&resource);
        assert!(clean_ansi_codes(&formatted).contains("READ"));
    }

    #[test]
    fn inline_json_string_changes_are_pretty_printed_and_labeled() {
        let mut resource = sample_update_resource();
        resource.attributes.clear();
        resource.changes = vec![Change {
            path: vec!["assume_role_policy".into()],
            before: Some(serde_json::json!(
                "{\"Statement\":[{\"Action\":\"sts:AssumeRole\",\"Effect\":\"Allow\"}]}"
            )),
            after: Some(serde_json::json!(
                "{\"Statement\":[{\"Action\":\"sts:TagSession\",\"Effect\":\"Allow\"}]}"
            )),
            sensitive: false,
            computed: false,
        }];
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let output = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        ));

        assert!(
            output.contains("~ assume_role_policy (inline JSON string)"),
            "{output}"
        );
        assert!(
            output
                .lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")
                    == "- \"Action\": \"sts:AssumeRole\","),
            "{output}"
        );
        assert!(
            output
                .lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")
                    == "+ \"Action\": \"sts:TagSession\","),
            "{output}"
        );
        assert!(!output.contains("{\"Statement\""), "{output}");
    }

    #[test]
    fn inline_json_arrow_attributes_are_pretty_printed_and_labeled() {
        let mut resource = sample_update_resource();
        resource.changes.clear();
        resource.attributes = [(
            "policy".to_string(),
            serde_json::json!("{\"Version\":\"2012-10-17\"} → {\"Version\":\"2012-10-18\"}"),
        )]
        .into_iter()
        .collect();
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let output = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        ));

        assert!(output.contains("~ policy (inline JSON string)"), "{output}");
        assert!(
            output
                .lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")
                    == "- \"Version\": \"2012-10-17\""),
            "{output}"
        );
        assert!(
            output
                .lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")
                    == "+ \"Version\": \"2012-10-18\""),
            "{output}"
        );
    }

    #[test]
    fn plain_string_changes_keep_side_by_side_layout() {
        let mut resource = sample_update_resource();
        resource.attributes.clear();
        resource.changes = vec![Change {
            path: vec!["instance_type".into()],
            before: Some(serde_json::json!("t3.micro")),
            after: Some(serde_json::json!("t3.small")),
            sensitive: false,
            computed: false,
        }];
        let plan = TerraformPlan {
            resources: vec![resource],
            ..Default::default()
        };
        let output = clean_ansi_codes(&format_terminal_output_with_options(
            &plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: true,
                show_full_attribution: false,
                color: false,
            },
        ));

        assert!(!output.contains("inline JSON string"));
        assert!(output.contains("- \"t3.micro\""), "{output}");
        assert!(output.contains("+ \"t3.small\""), "{output}");
        assert!(!output.contains("│"), "{output}");
    }

    #[test]
    fn test_format_summary_line_all_actions() {
        let summary = Summary {
            add: 5,
            change: 3,
            destroy: 2,
            read: 1,
        };

        let formatted = format_summary_line(&summary);
        assert!(clean_ansi_codes(&formatted).contains("5 to add"));
        assert!(clean_ansi_codes(&formatted).contains("3 to change"));
        assert!(clean_ansi_codes(&formatted).contains("2 to destroy"));
        assert!(clean_ansi_codes(&formatted).contains("1 to read"));
        assert!(clean_ansi_codes(&formatted).contains("│"));
    }

    #[test]
    fn test_format_summary_line_partial_actions() {
        let summary = Summary {
            add: 2,
            change: 0,
            destroy: 1,
            read: 0,
        };

        let formatted = format_summary_line(&summary);
        assert!(clean_ansi_codes(&formatted).contains("2 to add"));
        assert!(!clean_ansi_codes(&formatted).contains("to change"));
        assert!(clean_ansi_codes(&formatted).contains("1 to destroy"));
        assert!(!clean_ansi_codes(&formatted).contains("to read"));
    }
}
