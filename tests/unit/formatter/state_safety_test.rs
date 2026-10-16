use serde_json::json;
use std::collections::HashMap;
use tfdiff::{
    format_html_output, format_markdown_output, format_terminal_output, ActionType, Change,
    PlanMode, Resource, Summary, TerraformPlan,
};

#[test]
fn html_escapes_resource_paths_and_attribute_keys() {
    let mut attributes = HashMap::new();
    attributes.insert("<img src=x onerror=alert(1)>".to_string(), json!("safe"));
    let plan = TerraformPlan {
        mode: PlanMode::StateDiff,
        summary: Summary {
            change: 1,
            ..Default::default()
        },
        resources: vec![Resource {
            id: "test_resource.<script>alert(1)</script>".to_string(),
            plan_delta: None,
            name: "item".to_string(),
            type_name: "test_resource".to_string(),
            provider: "test".to_string(),
            action: ActionType::Update,
            changes: vec![Change {
                path: vec!["<svg/onload=alert(1)>".to_string()],
                before: Some(json!("before")),
                after: Some(json!("after")),
                sensitive: false,
                computed: false,
            }],
            attributes,
            audit_attribution: None,
            applied: false,
            source: None,
        }],
        ..Default::default()
    };
    let html = format_html_output(&plan);
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("&lt;svg/onload=alert(1)&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
}

#[test]
fn html_escapes_attribute_keys_in_fallback_diff_notations() {
    for notation in ["before → after", "+ added", "- removed"] {
        let mut attributes = HashMap::new();
        attributes.insert("<img src=x onerror=alert(1)>".to_string(), json!(notation));
        let plan = TerraformPlan {
            mode: PlanMode::StateDiff,
            resources: vec![Resource {
                id: "test_resource.item".to_string(),
                plan_delta: None,
                name: "item".to_string(),
                type_name: "test_resource".to_string(),
                provider: "test".to_string(),
                action: ActionType::Update,
                changes: vec![],
                attributes,
                audit_attribution: None,
                applied: false,
                source: None,
            }],
            ..Default::default()
        };
        let html = format_html_output(&plan);
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(!html.contains("<img src=x onerror=alert(1)>"));
    }
}

#[test]
fn html_escapes_data_source_ids() {
    let plan = TerraformPlan {
        data_sources: vec![tfdiff::DataSource {
            id: "data.test.<script>alert(1)</script>".to_string(),
            name: "item".to_string(),
            type_name: "test".to_string(),
            provider: "test".to_string(),
            attributes: HashMap::new(),
        }],
        ..Default::default()
    };
    let html = format_html_output(&plan);

    assert!(html.contains("data.test.&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
}

#[test]
fn html_escapes_attribute_keys_in_single_column_fallback() {
    let mut attributes = HashMap::new();
    attributes.insert("<img src=x onerror=alert(1)>".to_string(), json!("safe"));
    let plan = TerraformPlan {
        resources: vec![Resource {
            id: "test_resource.item".to_string(),
            plan_delta: None,
            name: "item".to_string(),
            type_name: "test_resource".to_string(),
            provider: "test".to_string(),
            action: ActionType::Update,
            changes: vec![],
            attributes,
            audit_attribution: None,
            applied: false,
            source: None,
        }],
        ..Default::default()
    };
    let html = format_html_output(&plan);
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
    assert!(!html.contains("<img src=x onerror=alert(1)>"));
}

#[test]
fn sensitive_changes_are_explicit_in_terminal_markdown_and_html() {
    let plan = TerraformPlan {
        mode: PlanMode::StateDiff,
        resources: vec![Resource {
            id: "test_resource.item".to_string(),
            plan_delta: None,
            name: "item".to_string(),
            type_name: "test_resource".to_string(),
            provider: "test".to_string(),
            action: ActionType::Update,
            changes: vec![Change {
                path: vec!["password".to_string()],
                before: Some(json!("(sensitive value)")),
                after: Some(json!("(sensitive value)")),
                sensitive: true,
                computed: false,
            }],
            attributes: HashMap::new(),
            audit_attribution: None,
            applied: false,
            source: None,
        }],
        ..Default::default()
    };
    assert!(format_terminal_output(&plan).contains("sensitive value changed"));
    assert!(format_markdown_output(&plan).contains("(sensitive value)"));
    assert!(format_html_output(&plan).contains("sensitive value changed"));
}

#[test]
fn partially_sensitive_composite_change_keeps_unmarked_siblings_visible() {
    let plan = TerraformPlan {
        mode: PlanMode::StateDiff,
        resources: vec![Resource {
            id: "test_resource.item".to_string(),
            plan_delta: None,
            name: "item".to_string(),
            type_name: "test_resource".to_string(),
            provider: "test".to_string(),
            action: ActionType::Update,
            changes: vec![
                Change {
                    path: vec!["items".to_string(), "[0]".to_string()],
                    before: Some(json!({
                        "secret":"(sensitive value)",
                        "public":"PUBLIC_A"
                    })),
                    after: Some(json!({
                        "secret":"(sensitive value)",
                        "public":"PUBLIC_B"
                    })),
                    sensitive: false,
                    computed: false,
                },
                Change {
                    path: vec!["items".to_string(), "[0]".to_string(), "secret".to_string()],
                    before: Some(json!("(sensitive value)")),
                    after: Some(json!("(sensitive value)")),
                    sensitive: true,
                    computed: false,
                },
            ],
            attributes: HashMap::new(),
            audit_attribution: None,
            applied: false,
            source: None,
        }],
        ..Default::default()
    };

    for output in [
        format_terminal_output(&plan),
        format_markdown_output(&plan),
        format_html_output(&plan),
    ] {
        assert!(output.contains("PUBLIC_A"));
        assert!(output.contains("PUBLIC_B"));
        assert!(output.contains("sensitive value changed"));
        assert!(!output.contains("SECRET_A"));
        assert!(!output.contains("SECRET_B"));
    }
}

#[test]
fn long_unicode_terminal_values_do_not_panic() {
    let value = format!("{}x", "🙂".repeat(30));
    let plan = TerraformPlan {
        resources: vec![Resource {
            id: "test_resource.item".to_string(),
            plan_delta: None,
            name: "item".to_string(),
            type_name: "test_resource".to_string(),
            provider: "test".to_string(),
            action: ActionType::Update,
            changes: vec![Change {
                path: vec!["label".to_string()],
                before: Some(json!(value)),
                after: Some(json!("short")),
                sensitive: false,
                computed: false,
            }],
            attributes: HashMap::new(),
            audit_attribution: None,
            applied: false,
            source: None,
        }],
        ..Default::default()
    };
    let output = format_terminal_output(&plan);
    assert!(output.contains("label"));
}
