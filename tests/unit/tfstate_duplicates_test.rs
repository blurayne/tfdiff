use tfdiff::tfstate_duplicates::{
    classify_histories, fingerprint, ClassifiedVersion, KeyHistory, VersionInput, VersionStatus,
};

fn body(lineage: &str, resource_id: &str, output: &str) -> String {
    serde_json::json!({
        "version": 4,
        "serial": 1,
        "lineage": lineage,
        "outputs": {"out": {"value": output, "type": "string"}},
        "resources": [
            {"mode": "managed", "type": "t", "name": "n", "instances": [{"attributes": {"id": resource_id}}]},
            {"mode": "data", "type": "d", "name": "n", "instances": [{"attributes": {"id": "data-1"}}]}
        ]
    })
    .to_string()
}

fn live(id: &str, latest: bool, content: &str) -> VersionInput {
    live_at(id, latest, content, None)
}

fn live_at(id: &str, latest: bool, content: &str, last_modified: Option<&str>) -> VersionInput {
    let hash = tfdiff::cache::sha256_hex(content.as_bytes());
    VersionInput {
        version_id: id.to_string(),
        last_modified: last_modified.map(str::to_string),
        is_delete_marker: false,
        is_latest: latest,
        content: Some(fingerprint(content.as_bytes()).map(|fp| (hash, fp))),
    }
}

fn marker(id: &str, latest: bool) -> VersionInput {
    VersionInput {
        version_id: id.to_string(),
        last_modified: None,
        is_delete_marker: true,
        is_latest: latest,
        content: None,
    }
}

fn classify_key(versions: Vec<VersionInput>) -> Vec<ClassifiedVersion> {
    classify_histories(&[KeyHistory {
        key: "dev/terraform.tfstate".into(),
        versions,
    }])
    .remove(0)
    .versions
}

fn statuses(result: &[ClassifiedVersion]) -> Vec<VersionStatus> {
    result.iter().map(|version| version.status).collect()
}

#[test]
fn identical_bytes_in_same_key_history_are_kept() {
    let content = body("a", "r1", "x");
    let result = classify_key(vec![
        live("v2", true, &content),
        live("v1", false, &content),
    ]);
    assert_eq!(
        statuses(&result),
        [VersionStatus::Current, VersionStatus::Kept]
    );
}

#[test]
fn different_content_same_lineage_is_kept() {
    let result = classify_key(vec![
        live("v2", true, &body("a", "r1", "x")),
        live("v1", false, &body("a", "r1", "y")),
    ]);
    assert_eq!(
        statuses(&result),
        [VersionStatus::Current, VersionStatus::Kept]
    );
    assert_eq!(
        result[1].reason.as_deref(),
        Some("no matching copy in another key")
    );
}

fn moved(old_body: &str, new_body: &str) -> Vec<ClassifiedVersion> {
    classify_histories(&[
        KeyHistory {
            key: "old/terraform.tfstate".into(),
            versions: vec![
                marker("m1", true),
                live_at("v1", false, old_body, Some("2026-01-01T00:00:00Z")),
            ],
        },
        KeyHistory {
            key: "new/terraform.tfstate".into(),
            versions: vec![live_at("w1", true, new_body, Some("2026-01-05T00:00:00Z"))],
        },
    ])
    .remove(0)
    .versions
}

#[test]
fn moved_state_with_same_resources_but_different_outputs_is_deprecated() {
    let old = moved(&body("a", "r1", "x"), &body("b", "r1", "y"));
    assert_eq!(old[1].status, VersionStatus::Deprecated);
    assert_eq!(
        old[1].reason.as_deref(),
        Some("same resources as new/terraform.tfstate @w1")
    );
}

#[test]
fn resource_match_ignores_data_sources_module_and_name() {
    let old = body("a", "r1", "x");
    let new = serde_json::json!({
        "version": 4,
        "lineage": "b",
        "resources": [
            {"mode": "managed", "module": "module.m", "type": "t", "name": "renamed", "instances": [{"attributes": {"id": "r1"}}]},
            {"mode": "data", "type": "d", "name": "n", "instances": [{"attributes": {"id": "data-2"}}]}
        ]
    })
    .to_string();
    assert_eq!(moved(&old, &new)[1].status, VersionStatus::Deprecated);
}

#[test]
fn states_without_managed_resources_never_match_by_resources() {
    let empty = |output: &str| {
        serde_json::json!({"version": 4, "lineage": "a", "outputs": {"o": {"value": output}}, "resources": []})
            .to_string()
    };
    let old = moved(&empty("x"), &empty("y"));
    assert_eq!(old[1].status, VersionStatus::Kept);
    assert!(old[1].matched_to.is_none());
}

#[test]
fn current_of_live_key_is_deprecated_by_newer_key_with_same_resources() {
    let result = classify_histories(&[
        KeyHistory {
            key: "dev/ecr/terraform.tfstate".into(),
            versions: vec![live_at(
                "v1",
                true,
                &body("a", "r1", "x"),
                Some("2026-01-01T00:00:00Z"),
            )],
        },
        KeyHistory {
            key: "dev/euc1/infrastructure/ecr.tfstate".into(),
            versions: vec![
                live_at(
                    "w3",
                    true,
                    &body("b", "r2", "y"),
                    Some("2026-01-09T00:00:00Z"),
                ),
                live_at(
                    "w2",
                    false,
                    &body("b", "r1", "y"),
                    Some("2026-01-05T00:00:00Z"),
                ),
                live_at(
                    "w1",
                    false,
                    &body("b", "r1", "x"),
                    Some("2026-01-02T00:00:00Z"),
                ),
            ],
        },
    ]);
    assert_eq!(result[0].versions[0].status, VersionStatus::Deprecated);
    assert_eq!(
        result[0].versions[0].reason.as_deref(),
        Some("same resources as dev/euc1/infrastructure/ecr.tfstate @w1")
    );
    assert_eq!(
        statuses(&result[1].versions),
        [
            VersionStatus::Current,
            VersionStatus::Kept,
            VersionStatus::Kept
        ]
    );
}

#[test]
fn of_identical_heads_the_newer_copy_is_deprecated() {
    let content = body("a", "r1", "x");
    let result = classify_histories(&[
        KeyHistory {
            key: "a/terraform.tfstate".into(),
            versions: vec![live_at("v1", true, &content, Some("2026-01-01T00:00:00Z"))],
        },
        KeyHistory {
            key: "b/terraform.tfstate".into(),
            versions: vec![live_at("w1", true, &content, Some("2026-01-02T00:00:00Z"))],
        },
    ]);
    // Identical heads: the older a is the original, the newer b is its deprecated copy.
    assert_eq!(result[0].versions[0].status, VersionStatus::Current);
    assert_eq!(result[1].versions[0].status, VersionStatus::Deprecated);
    assert_eq!(
        result[1].versions[0].reason.as_deref(),
        Some("copy of a/terraform.tfstate @v1")
    );
}

#[test]
fn moved_state_with_same_resources_is_deprecated_and_linked() {
    let result = classify_histories(&[
        KeyHistory {
            key: "old/terraform.tfstate".into(),
            versions: vec![
                live_at(
                    "v2",
                    true,
                    &body("a", "r9", "z"),
                    Some("2026-01-02T00:00:00Z"),
                ),
                live_at(
                    "v1",
                    false,
                    &body("a", "r1", "x"),
                    Some("2026-01-01T00:00:00Z"),
                ),
            ],
        },
        KeyHistory {
            key: "new/terraform.tfstate".into(),
            versions: vec![live_at(
                "w1",
                true,
                &body("b", "r1", "x"),
                Some("2026-01-05T00:00:00Z"),
            )],
        },
    ]);
    let old = &result[0].versions;
    assert_eq!(
        statuses(old),
        [VersionStatus::Current, VersionStatus::Deprecated]
    );
    assert_eq!(
        old[1].reason.as_deref(),
        Some("same resources as new/terraform.tfstate @w1")
    );
    assert_eq!(
        old[1].matched_to.as_ref().map(|m| m.version_id.as_str()),
        Some("w1")
    );
}

#[test]
fn lineage_change_with_different_resource_id_is_kept() {
    let result = classify_key(vec![
        live("v2", true, &body("b", "r1", "x")),
        live("v1", false, &body("a", "r2", "x")),
    ]);
    assert_eq!(
        statuses(&result),
        [VersionStatus::Current, VersionStatus::Kept]
    );
}

#[test]
fn lineage_change_with_different_output_value_is_kept() {
    let result = classify_key(vec![
        live("v2", true, &body("b", "r1", "x")),
        live("v1", false, &body("a", "r1", "y")),
    ]);
    assert_eq!(statuses(&result)[1], VersionStatus::Kept);
}

#[test]
fn latest_delete_marker_marks_markers_deleted_and_live_versions_kept() {
    let content = body("a", "r1", "x");
    let result = classify_key(vec![
        marker("m2", true),
        live("v2", false, &content),
        marker("m1", false),
        live("v1", false, &content),
    ]);
    assert_eq!(
        statuses(&result),
        [
            VersionStatus::Deleted,
            VersionStatus::Kept,
            VersionStatus::Deleted,
            VersionStatus::Kept
        ]
    );
    assert_eq!(
        result[1].reason.as_deref(),
        Some("key is deleted; history retained")
    );
}

#[test]
fn older_delete_marker_is_deleted_while_current_stays_current() {
    let content = body("a", "r1", "x");
    let result = classify_key(vec![
        live("v2", true, &content),
        marker("m1", false),
        live("v1", false, &content),
    ]);
    assert_eq!(
        statuses(&result),
        [
            VersionStatus::Current,
            VersionStatus::Deleted,
            VersionStatus::Kept
        ]
    );
}

#[test]
fn unreadable_bodies_are_kept_and_never_deprecated() {
    let content = body("a", "r1", "x");
    let result = classify_key(vec![
        live("v2", true, &content),
        live("v1", false, "{not json"),
    ]);
    assert_eq!(
        statuses(&result),
        [VersionStatus::Current, VersionStatus::Kept]
    );
    assert_eq!(result[1].reason.as_deref(), Some("unreadable state"));

    let result = classify_key(vec![
        live("v2", true, "{not json"),
        live("v1", false, &content),
    ]);
    assert_eq!(statuses(&result)[1], VersionStatus::Kept);
}

#[test]
fn current_is_the_latest_live_version() {
    let content = body("a", "r1", "x");
    let result = classify_key(vec![
        live("v3", true, &content),
        live("v2", false, &content),
        live("v1", false, &body("a", "r1", "z")),
    ]);
    assert_eq!(
        statuses(&result),
        [
            VersionStatus::Current,
            VersionStatus::Kept,
            VersionStatus::Kept
        ]
    );
}

#[test]
fn live_key_version_identical_to_newer_version_in_another_key_is_deprecated() {
    let content = body("a", "r1", "x");
    let result = classify_histories(&[
        KeyHistory {
            key: "old/terraform.tfstate".into(),
            versions: vec![
                live_at(
                    "v2",
                    true,
                    &body("a", "r9", "z"),
                    Some("2026-01-03T00:00:00Z"),
                ),
                live_at("v1", false, &content, Some("2026-01-01T00:00:00Z")),
            ],
        },
        KeyHistory {
            key: "new/terraform.tfstate".into(),
            versions: vec![
                live_at(
                    "w2",
                    true,
                    &body("b", "r2", "y"),
                    Some("2026-01-06T00:00:00Z"),
                ),
                live_at("w1", false, &content, Some("2026-01-05T00:00:00Z")),
            ],
        },
    ]);
    assert_eq!(
        statuses(&result[0].versions),
        [VersionStatus::Current, VersionStatus::Deprecated]
    );
    assert_eq!(
        result[0].versions[1].reason.as_deref(),
        Some("identical to new/terraform.tfstate @w1")
    );
    assert_eq!(
        statuses(&result[1].versions),
        [VersionStatus::Current, VersionStatus::Kept]
    );
}

#[test]
fn deleted_key_version_identical_to_newer_live_key_is_deprecated() {
    let content = body("a", "r1", "x");
    let result = classify_histories(&[
        KeyHistory {
            key: "old-directory-structure/terraform.tfstate".into(),
            versions: vec![
                marker("m1", true),
                live_at("v1", false, &content, Some("2026-01-01T00:00:00Z")),
            ],
        },
        KeyHistory {
            key: "dev/acm/terraform.tfstate".into(),
            versions: vec![live_at("v2", true, &content, Some("2026-01-05T00:00:00Z"))],
        },
    ]);
    assert_eq!(
        statuses(&result[0].versions),
        [VersionStatus::Deleted, VersionStatus::Deprecated]
    );
    assert_eq!(
        result[0].versions[1].reason.as_deref(),
        Some("identical to dev/acm/terraform.tfstate @v2")
    );
    assert_eq!(statuses(&result[1].versions), [VersionStatus::Current]);
}

#[test]
fn deleted_key_version_is_not_compared_with_older_live_key() {
    let content = body("a", "r1", "x");
    let result = classify_histories(&[
        KeyHistory {
            key: "old/terraform.tfstate".into(),
            versions: vec![
                marker("m1", true),
                live_at("v1", false, &content, Some("2026-01-06T00:00:00Z")),
            ],
        },
        KeyHistory {
            key: "dev/terraform.tfstate".into(),
            versions: vec![live_at("v2", true, &content, Some("2026-01-05T00:00:00Z"))],
        },
    ]);
    assert_eq!(statuses(&result[0].versions)[1], VersionStatus::Kept);
    assert_eq!(
        result[0].versions[1].reason.as_deref(),
        Some("key is deleted; history retained")
    );
}

#[test]
fn deleted_key_version_with_changed_resources_stays_kept() {
    let result = classify_histories(&[
        KeyHistory {
            key: "old/terraform.tfstate".into(),
            versions: vec![
                marker("m1", true),
                live_at(
                    "v1",
                    false,
                    &body("a", "r1", "x"),
                    Some("2026-01-01T00:00:00Z"),
                ),
            ],
        },
        KeyHistory {
            key: "dev/terraform.tfstate".into(),
            versions: vec![live_at(
                "v2",
                true,
                &body("b", "r2", "y"),
                Some("2026-01-05T00:00:00Z"),
            )],
        },
    ]);
    assert_eq!(statuses(&result[0].versions)[1], VersionStatus::Kept);
}

#[test]
fn fingerprint_identity_falls_back_to_arn_then_name() {
    let state = serde_json::json!({
        "version": 4,
        "resources": [
            {"mode": "managed", "type": "t", "name": "a", "instances": [{"attributes": {"id": "r1", "arn": "arn:1"}}]},
            {"mode": "managed", "type": "t", "name": "b", "instances": [{"attributes": {"arn": "arn:2", "name": "n2"}}]},
            {"mode": "managed", "type": "t", "name": "c", "instances": [{"attributes": {"name": "n3"}}]},
            {"mode": "managed", "type": "t", "name": "d", "instances": [{"attributes": {"id": ""}}]},
            {"mode": "data", "type": "d", "name": "e", "instances": [{"attributes": {"id": "data-1"}}]}
        ]
    })
    .to_string();
    let fp = fingerprint(state.as_bytes()).unwrap();
    assert_eq!(
        fp.managed_resources.iter().collect::<Vec<_>>(),
        ["t|arn:2", "t|n3", "t|r1"]
    );
}

#[test]
fn fingerprint_collects_resources_data_and_outputs() {
    let fp = fingerprint(body("a", "r1", "x").as_bytes()).unwrap();
    assert_eq!(fp.lineage.as_deref(), Some("a"));
    assert_eq!(fp.serial, Some(1));
    assert!(fp.resource_ids.contains("managed||t|n||r1"));
    assert!(fp.resource_ids.contains("data||d|n||data-1"));
    assert_eq!(fp.managed_resources.iter().collect::<Vec<_>>(), ["t|r1"]);
    assert_eq!(fp.outputs.len(), 1);
    assert!(fingerprint(b"").is_err());
    assert!(fingerprint(b"[]").is_err());
}

#[test]
fn sort_newest_first_orders_versions_and_keys_by_last_modified() {
    let content = body("a", "r1", "x");
    let mut keys = classify_histories(&[
        KeyHistory {
            key: "old/terraform.tfstate".into(),
            versions: vec![
                live_at("v2", true, &content, Some("2026-01-02T00:00:00Z")),
                live_at(
                    "v1",
                    false,
                    &body("a", "r9", "z"),
                    Some("2026-01-01T00:00:00Z"),
                ),
            ],
        },
        KeyHistory {
            key: "new/terraform.tfstate".into(),
            versions: vec![
                live_at(
                    "w2",
                    true,
                    &body("c", "r2", "y"),
                    Some("2026-01-09T00:00:00Z"),
                ),
                live_at("w1", false, &content, Some("2026-01-08T00:00:00Z")),
            ],
        },
    ]);
    tfdiff::tfstate_duplicates::sort_newest_first(&mut keys);
    assert_eq!(keys[0].key, "new/terraform.tfstate");
    assert_eq!(keys[1].key, "old/terraform.tfstate");
    let versions: Vec<_> = keys[0]
        .versions
        .iter()
        .map(|v| v.version_id.as_str())
        .collect();
    assert_eq!(versions, ["w2", "w1"]);
    let versions: Vec<_> = keys[1]
        .versions
        .iter()
        .map(|v| v.version_id.as_str())
        .collect();
    assert_eq!(versions, ["v2", "v1"]);
}
