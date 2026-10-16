use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;
use tempfile::NamedTempFile;

#[cfg(test)]
mod cli_integration_tests {
    use super::*;

    fn create_temp_file_with_content(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{}", content).unwrap();
        file
    }

    #[test]
    fn skip_ids_keeps_json_identity_attributes_and_pager_is_skipped_for_pipes() {
        let plan_content = r#"
Terraform used the selected providers to generate the following execution plan.

# aws_s3_bucket.test will be created
+ resource "aws_s3_bucket" "test" {
    + bucket = "test-bucket"
    + id     = "test-bucket-id"
  }

Plan: 1 to add, 0 to change, 0 to destroy.
"#;
        let file = create_temp_file_with_content(plan_content);
        let output = Command::cargo_bin("tfdiff")
            .unwrap()
            .arg(file.path())
            .args(["--format", "json", "--skip-ids", "--pager"])
            .env("TFDIFF_PAGER", "/nonexistent/tfdiff-pager")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(serialized.contains("test-bucket-id"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("could not start pager"));
    }

    #[test]
    fn plan_secrets_are_masked_unless_raw_secrets() {
        let plan_content = r#"
Terraform used the selected providers to generate the following execution plan.

# aws_db_instance.acme will be created
+ resource "aws_db_instance" "acme" {
    + identifier = "acme-db"
    + password   = "acme-secret-pw"
    + access_key = "AKIAIOSFODNN7EXAMPLE"
  }

Plan: 1 to add, 0 to change, 0 to destroy.
"#;
        let file = create_temp_file_with_content(plan_content);
        let run = |extra: &[&str]| {
            let output = Command::cargo_bin("tfdiff")
                .unwrap()
                .arg(file.path())
                .args(["--format", "json"])
                .args(extra)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };

        let masked = run(&[]);
        assert!(!masked.contains("acme-secret-pw"));
        assert!(!masked.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(masked.contains("(sensitive value)"));
        assert!(masked.contains("acme-db"));

        let raw = run(&["--raw-secrets"]);
        assert!(raw.contains("acme-secret-pw"));
        assert!(raw.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn test_cli_help() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("--help")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Compare Terraform plans and state snapshots",
            ))
            .stdout(predicate::str::contains("--format"));
    }

    #[test]
    fn test_cli_version() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("--version")
            .assert()
            .success()
            .stdout(predicate::str::contains("tfdiff"));
    }

    #[test]
    fn test_cli_with_file_input() {
        let plan_content = r#"
Terraform used the selected providers to generate the following execution plan.

# aws_s3_bucket.test will be created
+ resource "aws_s3_bucket" "test" {
    + bucket = "test-bucket"
    + id     = (known after apply)
  }

Plan: 1 to add, 0 to change, 0 to destroy.
        "#;

        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("TERRAFORM PLAN ANALYSIS").not())
            .stdout(predicate::str::contains("╔").not())
            .stdout(predicate::str::contains("1 resources to add"));

        Command::cargo_bin("tfdiff")
            .unwrap()
            .arg(file.path())
            .arg("--pretty")
            .assert()
            .success()
            .stdout(predicate::str::contains("TERRAFORM PLAN ANALYSIS"))
            .stdout(predicate::str::contains("╔"));
    }

    #[test]
    fn test_cli_json_format() {
        let plan_content = "Plan: 2 to add, 1 to change, 0 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--format")
            .arg("json")
            .assert()
            .success()
            .stdout(predicate::str::contains("mode"))
            .stdout(predicate::str::contains("summary"));
    }

    #[test]
    fn test_cli_html_format() {
        let plan_content = "Plan: 1 to add, 0 to change, 1 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--format")
            .arg("html")
            .assert()
            .success()
            .stdout(predicate::str::contains("<!DOCTYPE html>"))
            .stdout(predicate::str::contains("<title>Terraform changes</title>"));
    }

    #[test]
    fn test_cli_markdown_format() {
        let plan_content = "Plan: 0 to add, 2 to change, 0 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--format")
            .arg("markdown")
            .assert()
            .success()
            .stdout(predicate::str::contains("# Terraform Plan"));
    }

    #[test]
    fn test_cli_web_mode() {
        let plan_content = "Plan: 1 to add, 1 to change, 1 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--web")
            .arg("--port")
            .arg("9999")
            .assert()
            .success()
            .stderr(predicate::str::contains("Web UI mode on port 9999"));
    }

    #[test]
    fn test_cli_filter_option() {
        let plan_content = "Plan: 1 to add, 1 to change, 1 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--filter")
            .arg("create,update")
            .assert()
            .success();
    }

    #[test]
    fn test_cli_verbose_mode() {
        let plan_content = "Plan: 1 to add, 0 to change, 0 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path()).arg("--verbose").assert().success();
    }

    #[test]
    fn test_cli_quiet_mode() {
        let plan_content = "Plan: 1 to add, 0 to change, 0 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path()).arg("--quiet").assert().success();
    }

    #[test]
    fn test_cli_summary_mode() {
        let plan_content = "Plan: 5 to add, 3 to change, 2 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path()).arg("--summary").assert().success();
    }

    #[test]
    fn test_cli_watch_mode() {
        let plan_content = "Plan: 1 to add, 0 to change, 0 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path()).arg("--watch").assert().success();
    }

    #[test]
    fn test_cli_nonexistent_file() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("/nonexistent/file.txt")
            .assert()
            .failure() // File not found should return error
            .stderr(predicate::str::contains("No such file or directory"));
    }

    #[test]
    fn test_cli_stdin_mode() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("--pretty")
            .write_stdin("Plan: 0 to add, 0 to change, 0 to destroy.")
            .assert()
            .success()
            .stdout(predicate::str::contains("TERRAFORM PLAN ANALYSIS"));
    }

    #[test]
    fn test_cli_invalid_format() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("--format").arg("invalid").assert().failure();
    }

    #[test]
    fn test_cli_invalid_port() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("--port").arg("invalid").assert().failure();
    }

    #[test]
    fn test_cli_conflicting_options() {
        let plan_content = "Plan: 1 to add, 0 to change, 0 to destroy.";
        let file = create_temp_file_with_content(plan_content);

        // Quiet and verbose should both be accepted (implementation decides precedence)
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--quiet")
            .arg("--verbose")
            .assert()
            .success();
    }

    #[test]
    fn test_cli_browser_mode() {
        let plan_content = r#"
Terraform used the selected providers to generate the following execution plan.

# aws_s3_bucket.test will be created
+ resource "aws_s3_bucket" "test" {
    + bucket = "test-bucket"
    + id     = (known after apply)
  }

Plan: 1 to add, 0 to change, 0 to destroy.
        "#;
        let file = create_temp_file_with_content(plan_content);

        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg(file.path())
            .arg("--browser")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "🌐 Opened Terraform diff in browser",
            ));
    }

    #[test]
    fn test_cli_browser_flag_available() {
        let mut cmd = Command::cargo_bin("tfdiff").unwrap();
        cmd.arg("--help")
            .assert()
            .success()
            .stdout(predicate::str::contains("--browser"))
            .stdout(predicate::str::contains(
                "Generate HTML and open in browser",
            ));
    }

    fn compact_error(args: &[&str], expected: &str) {
        let output = Command::cargo_bin("tfdiff")
            .unwrap()
            .env("AWS_ENDPOINT_URL_S3", "http://127.0.0.1:9")
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }

    #[test]
    fn compact_rejects_invalid_combinations_without_network() {
        let shape = "--compact requires exactly one S3 prefix, glob, or exact object input";
        compact_error(&["--compact", "local.tfstate"], shape);
        compact_error(
            &["--compact", "s3://b/a.tfstate", "s3://b/c.tfstate"],
            shape,
        );
        compact_error(
            &["--compact", "s3://b/a.tfstate@abc"],
            "--compact cannot be combined with an explicit S3 version selector; use --since/--until",
        );
        let options =
            "--compact cannot be combined with --skip-deleted, --full-inventory, or --sort-order";
        compact_error(
            &["--compact", "--skip-deleted", "s3://b/**/*.tfstate"],
            options,
        );
        compact_error(
            &["--compact", "--full-inventory", "s3://b/**/*.tfstate"],
            options,
        );
        compact_error(
            &["--compact", "--sort-order", "asc", "s3://b/**/*.tfstate"],
            options,
        );
        let authors = "--compact does not support --show-authors or --show-atomic-authors";
        compact_error(
            &["--compact", "--show-authors", "s3://b/**/*.tfstate"],
            authors,
        );
        compact_error(
            &["--compact", "--show-atomic-authors", "s3://b/**/*.tfstate"],
            authors,
        );
        compact_error(
            &[
                "--compact",
                "--generate-tfplan-file",
                "out.json",
                "s3://b/**/*.tfstate",
            ],
            "--compact cannot be combined with --generate-tfplan-file",
        );
        compact_error(
            &["--show-atomic-sources", "s3://b/**/*.tfstate"],
            "--show-atomic-sources requires --compact",
        );
    }

    #[test]
    fn only_state_migrations_requires_inventory_input_without_compact() {
        let shape = "--only-state-migrations requires an S3 prefix or glob input without --compact";
        compact_error(&["--only-state-migrations", "local.tfstate"], shape);
        compact_error(&["--only-state-migrations", "s3://b/a.tfstate"], shape);
        compact_error(
            &[
                "--only-state-migrations",
                "s3://b/a.tfstate",
                "s3://b/c.tfstate",
            ],
            shape,
        );
        compact_error(
            &[
                "--only-state-migrations",
                "--compact",
                "s3://b/**/*.tfstate",
            ],
            shape,
        );
    }

    #[test]
    fn format_shortcuts_select_output_format() {
        let file = create_temp_file_with_content("Plan: 1 to add, 0 to change, 0 to destroy.");
        for (flag, expected) in [
            ("--json", "\"summary\""),
            ("--md", "# Terraform Plan"),
            ("--html", "<!DOCTYPE html>"),
        ] {
            Command::cargo_bin("tfdiff")
                .unwrap()
                .arg(file.path())
                .arg(flag)
                .assert()
                .success()
                .stdout(predicate::str::contains(expected));
        }
        Command::cargo_bin("tfdiff")
            .unwrap()
            .arg(file.path())
            .args(["--json", "--md"])
            .assert()
            .failure();
    }

    #[test]
    fn resource_filters_select_resources_by_glob_or_regex() {
        let plan_content = r#"
Terraform used the selected providers to generate the following execution plan.

# aws_s3_bucket.logs will be created
+ resource "aws_s3_bucket" "logs" {
    + bucket = "logs-bucket"
    + id     = "logs-bucket"
  }

# aws_iam_role.app will be created
+ resource "aws_iam_role" "app" {
    + name = "app"
    + id   = "app-role"
  }

Plan: 2 to add, 0 to change, 0 to destroy.
"#;
        let file = create_temp_file_with_content(plan_content);
        let run = |args: &[&str]| {
            let output = Command::cargo_bin("tfdiff")
                .unwrap()
                .arg(file.path())
                .arg("--json")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            json["resources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|resource| resource["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            run(&["--filter-resource-name", "aws_s3_*"]),
            ["aws_s3_bucket.logs"]
        );
        assert_eq!(
            run(&["--regex", "--filter-resource-name", "aws_(iam|kms)_.*"]),
            ["aws_iam_role.app"]
        );
        assert!(run(&["--filter-resource-name", "google_*"]).is_empty());
        assert_eq!(
            run(&["--filter-resource-id", "app-*"]),
            ["aws_iam_role.app"]
        );

        Command::cargo_bin("tfdiff")
            .unwrap()
            .arg(file.path())
            .args(["--regex", "--filter-resource-name", "aws_("])
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid regex pattern 'aws_('"));
    }
}
