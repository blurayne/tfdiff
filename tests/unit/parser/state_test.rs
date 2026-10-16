use serde_json::json;
use tfdiff::compare_states;

fn state(resources: serde_json::Value) -> String {
    json!({
        "version": 4,
        "terraform_version": "1.8.0",
        "serial": 1,
        "lineage": "synthetic",
        "outputs": {},
        "resources": resources
    })
    .to_string()
}

fn resource(name: &str, mode: &str, instances: serde_json::Value) -> serde_json::Value {
    json!({
        "mode": mode,
        "type": "test_item",
        "name": name,
        "provider": "provider[\"registry.terraform.io/hashicorp/test\"]",
        "instances": instances
    })
}

#[test]
fn accepts_empty_and_identical_snapshots() {
    let empty = state(json!([]));
    let result = compare_states(&empty, &empty).unwrap();
    assert!(result.plan.warnings.is_empty());
    assert_eq!(
        result.plan.summary.add + result.plan.summary.change + result.plan.summary.destroy,
        0
    );
    let populated = state(json!([resource(
        "same",
        "managed",
        json!([{"attributes":{"nullable":null}}])
    )]));
    assert!(compare_states(&populated, &populated)
        .unwrap()
        .plan
        .resources
        .is_empty());
}

#[test]
fn detects_absent_versus_null_and_atomic_array_update() {
    let old = state(json!([resource(
        "one",
        "managed",
        json!([{"attributes":{"object":{"n":null},"list":[1,2]}}])
    )]));
    let new = state(json!([resource(
        "one",
        "managed",
        json!([{"attributes":{"object":{},"list":[2,1]}}])
    )]));
    let result = compare_states(&old, &new).unwrap();
    let changes = &result.plan.resources[0].changes;
    assert!(changes.iter().any(|change| change.path == ["object", "n"]
        && change.before == Some(serde_json::Value::Null)
        && change.after.is_none()));
    assert!(changes.iter().any(|change| change.path == ["list"]));
}

#[test]
fn rejects_sensitive_paths_that_do_not_apply_to_their_own_instance() {
    let cases = [
        (
            json!({"secret":"PLAINTEXT"}),
            json!([[{"type":"get_attr","value":"missing"}]]),
        ),
        (
            json!({"items":["PLAINTEXT"]}),
            json!([[{"type":"get_attr","value":"items"},{"type":"index","value":1}]]),
        ),
        (
            json!({"items":{"secret":"PLAINTEXT"}}),
            json!([[{"type":"get_attr","value":"items"},{"type":"index","value":0}]]),
        ),
    ];
    for (attributes, sensitive_attributes) in cases {
        let invalid = state(json!([resource(
            "bad",
            "managed",
            json!([{"attributes":attributes,"sensitive_attributes":sensitive_attributes}])
        )]));
        let empty = state(json!([]));
        let error = compare_states(&invalid, &empty).unwrap_err().to_string();
        assert!(!error.contains("PLAINTEXT"));
    }
}

#[test]
fn accepts_sensitive_paths_absent_only_on_opposite_snapshot() {
    let older = state(json!([resource(
        "one",
        "managed",
        json!([{
            "attributes":{"secret":"PLAINTEXT"},
            "sensitive_attributes":[[{"type":"get_attr","value":"secret"}]]
        }])
    )]));
    let later = state(json!([resource(
        "one",
        "managed",
        json!([{"attributes":{"public":"visible"}}])
    )]));
    let comparison = compare_states(&older, &later).unwrap();
    let serialized = serde_json::to_string(&comparison.plan).unwrap();
    assert!(!serialized.contains("PLAINTEXT"));
    assert!(serialized.contains("visible"));
}

#[test]
fn validates_duplicate_addresses_and_ignores_deposed_instances() {
    let duplicate = state(json!([
        resource("same", "managed", json!([{"attributes":{}}])),
        resource("same", "managed", json!([{"attributes":{}}]))
    ]));
    assert!(compare_states(&duplicate, &state(json!([]))).is_err());

    let deposed = state(json!([resource(
        "same",
        "managed",
        json!([{"deposed":"deadbeef","attributes":{"id":"retired"}}])
    )]));
    let comparison = compare_states(&deposed, &state(json!([]))).unwrap();
    assert_eq!(comparison.plan.summary.destroy, 0);
    assert!(comparison.plan.resources.is_empty());
    assert!(comparison
        .plan
        .warnings
        .iter()
        .any(|warning| warning.message.contains("Ignored deposed instances")));
}

#[test]
fn malformed_sensitivity_metadata_fails_closed_without_values() {
    let bad = state(json!([resource(
        "secret",
        "managed",
        json!([{
            "attributes":{"token":"do-not-leak"},
            "sensitive_attributes":[{"type":"unsupported"}]
        }])
    )]));
    let error = compare_states(&state(json!([])), &bad)
        .unwrap_err()
        .to_string();
    assert!(!error.contains("do-not-leak"));
}
