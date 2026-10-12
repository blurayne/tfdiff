use assert_cmd::Command;
use serde_json::{json, Value};
use std::fs;
use tempfile::tempdir;
use tfdiff::compare_states;

fn synthetic_state(attributes: Value) -> String {
    json!({
        "version": 4,
        "terraform_version": "1.8.0",
        "serial": 1,
        "lineage": "test-lineage",
        "outputs": {},
        "resources": [{
            "mode": "managed",
            "type": "test_resource",
            "name": "item",
            "provider": "provider[\"registry.terraform.io/hashicorp/test\"]",
            "instances": [{"attributes": attributes}]
        }]
    })
    .to_string()
}

fn empty_state() -> String {
    json!({
        "version": 4,
        "terraform_version": "1.8.0",
        "serial": 1,
        "lineage": "test-lineage",
        "outputs": {},
        "resources": []
    })
    .to_string()
}

#[test]
fn positional_and_explicit_diff_render_observational_comparison() {
    let directory = tempdir().unwrap();
    let old = directory.path().join("old.tfstate");
    let new = directory.path().join("new.tfstate");
    fs::write(&old, synthetic_state(json!({"name":"before"}))).unwrap();
    fs::write(&new, synthetic_state(json!({"name":"after"}))).unwrap();

    let positional = Command::cargo_bin("tfdiff")
        .unwrap()
        .args([old.to_str().unwrap(), new.to_str().unwrap()])
        .output()
        .unwrap();
    let explicit = Command::cargo_bin("tfdiff")
        .unwrap()
        .args(["diff", old.to_str().unwrap(), new.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(positional.status.success());
    assert!(explicit.status.success());
    let positional = String::from_utf8_lossy(&positional.stdout);
    let explicit = String::from_utf8_lossy(&explicit.stdout);
    assert!(positional.contains("TERRAFORM STATE SNAPSHOT DIFF"));
    assert!(positional.contains("Older Snapshot") || positional.contains("test_resource.item"));
    assert!(explicit.contains("TERRAFORM STATE SNAPSHOT DIFF"));
    assert!(positional.contains("1 resources observed as changed"));
}

#[test]
fn export_is_separate_complete_and_create_new() {
    let directory = tempdir().unwrap();
    let old = directory.path().join("old.tfstate");
    let new = directory.path().join("new.tfstate");
    let export = directory.path().join("diff.json");
    fs::write(&old, synthetic_state(json!({"name":"old","retained":true}))).unwrap();
    fs::write(&new, synthetic_state(json!({"name":"new","retained":true}))).unwrap();

    Command::cargo_bin("tfdiff")
        .unwrap()
        .args([
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "--filter",
            "update",
            "--generate-tfplan-file",
            export.to_str().unwrap(),
        ])
        .assert()
        .success();
    let value: Value = serde_json::from_slice(&fs::read(&export).unwrap()).unwrap();
    assert_eq!(value["format_version"], "1.2");
    assert_eq!(
        value["resource_changes"][0]["change"]["actions"],
        json!(["update"])
    );
    assert_eq!(
        value["resource_changes"][0]["change"]["before"]["retained"],
        true
    );

    Command::cargo_bin("tfdiff")
        .unwrap()
        .args([
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "--generate-tfplan-file",
            export.to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn partially_sensitive_array_object_keeps_public_sibling_changes_in_every_human_format() {
    let old = synthetic_state(json!({
        "items":[{"secret":"SECRET_A","public":"PUBLIC_A"}]
    }));
    let new = synthetic_state(json!({
        "items":[{"secret":"SECRET_B","public":"PUBLIC_B"}]
    }));
    let old: Value = serde_json::from_str(&old).unwrap();
    let new: Value = serde_json::from_str(&new).unwrap();
    let mut old = old;
    let mut new = new;
    let sensitive_path = json!([[
        {"type":"get_attr","value":"items"},
        {"type":"index","value":0},
        {"type":"get_attr","value":"secret"}
    ]]);
    old["resources"][0]["instances"][0]["sensitive_attributes"] = sensitive_path.clone();
    new["resources"][0]["instances"][0]["sensitive_attributes"] = sensitive_path;
    let old = old.to_string();
    let new = new.to_string();
    let comparison = compare_states(&old, &new).unwrap();
    let resource = &comparison.plan.resources[0];
    let serialized = serde_json::to_string(resource).unwrap();
    assert!(!serialized.contains("SECRET_A"));
    assert!(!serialized.contains("SECRET_B"));
    assert!(serialized.contains("PUBLIC_A"));
    assert!(serialized.contains("PUBLIC_B"));
    assert!(resource.changes.iter().any(|change| change.sensitive));
    assert!(resource
        .changes
        .iter()
        .any(|change| change.path == ["items", "[0]"] && !change.sensitive));

    let plan = &comparison.plan;
    let terminal = tfdiff::format_terminal_output(plan);
    let markdown = tfdiff::format_markdown_output(plan);
    let html = tfdiff::format_html_output(plan);
    for output in [&terminal, &markdown, &html] {
        assert!(!output.contains("SECRET_A"));
        assert!(!output.contains("SECRET_B"));
        assert!(output.contains("PUBLIC_A"));
        assert!(output.contains("PUBLIC_B"));
        assert!(output.contains("sensitive value changed"));
    }
}

#[test]
fn empty_objects_are_reported_alongside_siblings_on_creation_and_deletion() {
    let empty = empty_state();
    let populated = synthetic_state(json!({"empty": {}, "visible": 1}));

    for (older, later) in [(&empty, &populated), (&populated, &empty)] {
        let comparison = compare_states(older, later).unwrap();
        let resource = &comparison.plan.resources[0];
        assert!(resource.changes.iter().any(|change| {
            change.path == ["empty"]
                && if resource.action == tfdiff::ActionType::Create {
                    change.before.is_none() && change.after == Some(json!({}))
                } else {
                    change.before == Some(json!({})) && change.after.is_none()
                }
        }));
        assert!(resource
            .changes
            .iter()
            .any(|change| change.path == ["visible"]));

        for output in [
            tfdiff::format_terminal_output(&comparison.plan),
            tfdiff::format_markdown_output(&comparison.plan),
            tfdiff::format_html_output(&comparison.plan),
        ] {
            assert!(output.contains("empty"));
            assert!(output.contains("visible"));
            assert!(output.contains("1"));
        }
    }
}

#[test]
fn rejects_export_in_legacy_mode_and_too_many_paths() {
    Command::cargo_bin("tfdiff")
        .unwrap()
        .args(["--generate-tfplan-file", "unused.json"])
        .write_stdin("Plan: 0 to add, 0 to change, 0 to destroy.")
        .assert()
        .failure();
    Command::cargo_bin("tfdiff")
        .unwrap()
        .args(["a", "b", "c"])
        .assert()
        .failure();
}
