use tfdiff::models::*;
use tfdiff::parser::diff::*;

#[cfg(test)]
mod diff_parser_tests {
    use super::*;

    #[test]
    fn test_parse_resource_changes_create() {
        let lines = vec![
            "# aws_s3_bucket.example will be created",
            "+ resource \"aws_s3_bucket\" \"example\" {",
            "+   bucket = \"test-bucket\"",
            "+   id     = (known after apply)",
            "  }",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 1);

        let resource = &resources[0];
        assert_eq!(resource.name, "example");
        assert_eq!(resource.type_name, "aws_s3_bucket");
        assert!(matches!(resource.action, ActionType::Create));
        assert_eq!(resource.id, "aws_s3_bucket.example");
    }

    #[test]
    fn test_parse_resource_changes_update() {
        let lines = vec![
            "# aws_instance.web will be updated in-place",
            "~ resource \"aws_instance\" \"web\" {",
            "~   instance_type = \"t2.micro\" -> \"t3.small\"",
            "    tags          = {",
            "        \"Name\" = \"WebServer\"",
            "    }",
            "  }",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 1);

        let resource = &resources[0];
        assert_eq!(resource.name, "web");
        assert_eq!(resource.type_name, "aws_instance");
        assert!(matches!(resource.action, ActionType::Update));
    }

    #[test]
    fn test_parse_resource_changes_destroy() {
        let lines = vec![
            "# aws_security_group.legacy will be destroyed",
            "- resource \"aws_security_group\" \"legacy\" {",
            "-   description = \"Legacy security group\"",
            "-   id          = \"sg-12345678\"",
            "  }",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 1);

        let resource = &resources[0];
        assert_eq!(resource.name, "legacy");
        assert_eq!(resource.type_name, "aws_security_group");
        assert!(matches!(resource.action, ActionType::Destroy));
    }

    #[test]
    fn test_parse_resource_changes_multiple_resources() {
        let lines = vec![
            "# aws_s3_bucket.first will be created",
            "+ resource \"aws_s3_bucket\" \"first\" {",
            "+   bucket = \"first-bucket\"",
            "  }",
            "",
            "# aws_instance.second will be updated in-place",
            "~ resource \"aws_instance\" \"second\" {",
            "~   instance_type = \"t2.micro\" -> \"t3.small\"",
            "  }",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 2);

        assert_eq!(resources[0].name, "first");
        assert!(matches!(resources[0].action, ActionType::Create));

        assert_eq!(resources[1].name, "second");
        assert!(matches!(resources[1].action, ActionType::Update));
    }

    #[test]
    fn test_parse_resource_changes_no_resources() {
        let lines = vec![
            "Terraform will perform the following actions:",
            "",
            "Plan: 0 to add, 0 to change, 0 to destroy.",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 0);
    }

    #[test]
    fn test_parse_resource_changes_malformed_lines() {
        let lines = vec![
            "Some random output",
            "# malformed resource line without proper format",
            "+ some attribute = value",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 0);
    }

    #[test]
    fn test_parse_multiline_array_removal() {
        let lines = vec![
            "# aws_iam_policy_attachment.p-dpd0-backup_CHS3p-dpd0-backupSinkUserPA_52F38803 (p-dpd0-backup/CHS3p-dpd0-backupSinkUserPA) will be updated in-place",
            "~ resource \"aws_iam_policy_attachment\" \"p-dpd0-backup_CHS3p-dpd0-backupSinkUserPA_52F38803\" {",
            "      id         = \"CHS3p-dpd0-backupSinkUserPA\"",
            "      name       = \"CHS3p-dpd0-backupSinkUserPA\"",
            "    ~ roles      = [",
            "        - \"scout-aps1-apps-clickhouse-sa-role\",",
            "          # (1 unchanged element hidden)",
            "      ]",
            "      # (3 unchanged attributes hidden)",
            "  }",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 1);

        let resource = &resources[0];
        assert_eq!(resource.action, ActionType::Update);
        assert_eq!(resource.type_name, "aws_iam_policy_attachment");

        // The key assertion: the roles change should be parsed
        assert!(
            !resource.changes.is_empty(),
            "Should have parsed the roles array change"
        );

        let roles_change = resource
            .changes
            .iter()
            .find(|c| c.path.contains(&"roles".to_string()))
            .expect("Should have a change for 'roles'");

        // Before should contain the removed element
        assert!(
            roles_change.before.is_some(),
            "Before should have the removed element"
        );
        let before = roles_change.before.as_ref().unwrap();
        assert!(before.is_array(), "Before should be an array");
        let before_arr = before.as_array().unwrap();
        assert!(
            before_arr
                .iter()
                .any(|v| v.as_str() == Some("scout-aps1-apps-clickhouse-sa-role")),
            "Before array should contain the removed role"
        );

        // After should exist (with unchanged elements marker)
        assert!(roles_change.after.is_some(), "After should exist");
    }

    #[test]
    fn test_parse_multiline_array_with_additions() {
        let lines = vec![
            "# aws_security_group.example will be updated in-place",
            "~ resource \"aws_security_group\" \"example\" {",
            "    ~ ingress = [",
            "        + \"10.0.0.0/8\",",
            "        - \"192.168.0.0/16\",",
            "          # (2 unchanged elements hidden)",
            "      ]",
            "  }",
        ];

        let resources = parse_resource_changes(&lines);
        assert_eq!(resources.len(), 1);

        let resource = &resources[0];
        assert!(
            !resource.changes.is_empty(),
            "Should have parsed the ingress array change"
        );

        let change = &resource.changes[0];
        assert_eq!(change.path, vec!["ingress"]);

        // Before should contain removed element + unchanged marker
        let before = change.before.as_ref().unwrap().as_array().unwrap();
        assert!(before.iter().any(|v| v.as_str() == Some("192.168.0.0/16")));

        // After should contain added element + unchanged marker
        let after = change.after.as_ref().unwrap().as_array().unwrap();
        assert!(after.iter().any(|v| v.as_str() == Some("10.0.0.0/8")));
    }

    #[test]
    fn test_parse_resource_action_line_variations() {
        let test_cases = vec![
            (
                "# aws_s3_bucket.example will be created",
                Some(ActionType::Create),
            ),
            (
                "# aws_instance.server will be updated",
                Some(ActionType::Update),
            ),
            (
                "# aws_security_group.sg will be destroyed",
                Some(ActionType::Destroy),
            ),
            ("# some invalid format", None),
            ("not a resource line", None),
        ];

        for (line, expected) in test_cases {
            let lines = vec![line];
            let resources = parse_resource_changes(&lines);

            match expected {
                Some(expected_action) => {
                    assert_eq!(resources.len(), 1);
                    assert_eq!(resources[0].action, expected_action);
                }
                None => {
                    assert_eq!(resources.len(), 0);
                }
            }
        }
    }
}
