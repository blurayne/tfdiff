use tfdiff::tfstate_branches::{
    build, format_json, format_terminal, BranchReport, DeprecatedKind, KeyStatus, ObsoleteKind,
};
use tfdiff::tfstate_duplicates::{classify_histories, fingerprint, KeyHistory, VersionInput};

/// A state with one managed resource per identity (`type|identity` is `t|{id}`).
fn body(lineage: &str, ids: &[&str], output: &str) -> String {
    let resources: Vec<_> = ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "mode": "managed", "type": "t", "name": format!("n-{id}"),
                "instances": [{"attributes": {"id": id, "secret": "attribute-content"}}]
            })
        })
        .collect();
    serde_json::json!({
        "version": 4,
        "serial": 1,
        "lineage": lineage,
        "outputs": {"out": {"value": output, "type": "string"}},
        "resources": resources
    })
    .to_string()
}

fn live(id: &str, latest: bool, content: &str, last_modified: &str) -> VersionInput {
    let hash = tfdiff::cache::sha256_hex(content.as_bytes());
    VersionInput {
        version_id: id.to_string(),
        last_modified: Some(last_modified.to_string()),
        is_delete_marker: false,
        is_latest: latest,
        content: Some(fingerprint(content.as_bytes()).map(|fp| (hash, fp))),
    }
}

fn marker(id: &str, latest: bool, last_modified: &str) -> VersionInput {
    VersionInput {
        version_id: id.to_string(),
        last_modified: Some(last_modified.to_string()),
        is_delete_marker: true,
        is_latest: latest,
        content: None,
    }
}

fn history(key: &str, versions: Vec<VersionInput>) -> KeyHistory {
    KeyHistory {
        key: key.to_string(),
        versions,
    }
}

fn report(histories: &[KeyHistory]) -> BranchReport {
    let mut report = build(histories, classify_histories(histories));
    report.bucket = "b".to_string();
    report
}

fn status<'a>(report: &'a BranchReport, key: &str) -> &'a KeyStatus {
    &report
        .keys
        .iter()
        .find(|node| node.key == key)
        .unwrap_or_else(|| panic!("no key {key}"))
        .status
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-10T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn render(report: &BranchReport, verbose: bool) -> String {
    format_terminal(report, verbose, false, false, now())
}

/// The 16-character resource hash of a `body()` with these ids.
fn h(ids: &[&str]) -> String {
    let mut identities: Vec<String> = ids.iter().map(|id| format!("t|{id}")).collect();
    identities.sort();
    tfdiff::cache::sha256_hex(identities.join("\n").as_bytes())[..16].to_string()
}

const IDS: [&str; 11] = [
    "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11",
];

fn split_histories() -> Vec<KeyHistory> {
    vec![
        history(
            "old/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &IDS, "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "new/infra.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &IDS[..8], "y"),
                "2026-01-05T00:00:00Z",
            )],
        ),
        history(
            "new/app.tfstate",
            vec![live(
                "x1",
                true,
                &body("c", &IDS[8..], "z"),
                "2026-01-08T00:00:00Z",
            )],
        ),
    ]
}

#[test]
fn deleted_key_stays_deleted_even_when_fully_covered() {
    let content = body("a", &["r1", "r2"], "x");
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![
                marker("m1", true, "2026-01-03T00:00:00Z"),
                live("v1", false, &content, "2026-01-01T00:00:00Z"),
            ],
        ),
        history(
            "new/terraform.tfstate",
            vec![live("w1", true, &content, "2026-01-05T00:00:00Z")],
        ),
    ]);
    // A delete marker wins, even though every resource lives on elsewhere.
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Deleted {
            elsewhere: 2,
            of: 2
        }
    );
    assert_eq!(report.edges.len(), 1);
    let edge = &report.edges[0];
    assert_eq!((edge.resources, edge.of), (2, 2));
    assert!(edge.identical);
    assert_eq!(edge.from.version_id, "v1");
    assert_eq!(edge.to.version_id, "w1");
}

#[test]
fn of_identical_live_copies_the_newest_is_deprecated() {
    let content = body("a", &["r1", "r2"], "x");
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![live("v1", true, &content, "2026-01-01T00:00:00Z")],
        ),
        history(
            "new/terraform.tfstate",
            vec![live("w1", true, &content, "2026-01-05T00:00:00Z")],
        ),
        history(
            "newest/terraform.tfstate",
            vec![live("x1", true, &content, "2026-01-07T00:00:00Z")],
        ),
    ]);
    assert_eq!(status(&report, "old/terraform.tfstate"), &KeyStatus::Active);
    for copy in ["new/terraform.tfstate", "newest/terraform.tfstate"] {
        assert_eq!(
            status(&report, copy),
            &KeyStatus::Deprecated {
                kind: DeprecatedKind::CopyOf {
                    original: "old/terraform.tfstate".to_string()
                }
            }
        );
    }
}

#[test]
fn identical_bytes_in_an_older_version_of_a_newer_key_obsolete_the_source() {
    // new moved on after the copy, so old is not a copy of new's current state but superseded.
    let content = body("a", &["r1", "r2"], "x");
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![live("v1", true, &content, "2026-01-01T00:00:00Z")],
        ),
        history(
            "new/terraform.tfstate",
            vec![
                live(
                    "w2",
                    true,
                    &body("a", &["r1", "r2", "r3"], "y"),
                    "2026-01-06T00:00:00Z",
                ),
                live("w1", false, &content, "2026-01-05T00:00:00Z"),
            ],
        ),
    ]);
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Obsolete {
            kind: ObsoleteKind::MovedIdentical
        }
    );
    assert_eq!(status(&report, "new/terraform.tfstate"), &KeyStatus::Active);
    // The match is an older version of new, so new's HEAD follows it.
    let output = render(&report, false);
    assert!(
        output.contains("↳ new/terraform.tfstate @w1  5 days ago  → @w2 HEAD  4 days ago\n"),
        "{output}"
    );
}

#[test]
fn matching_head_is_preferred_over_an_earlier_matching_version() {
    // new still holds old's resources at its HEAD, so the HEAD is named, not w1.
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1", "r2"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "new/terraform.tfstate",
            vec![
                live(
                    "w2",
                    true,
                    &body("b", &["r1", "r2"], "z"),
                    "2026-01-06T00:00:00Z",
                ),
                live(
                    "w1",
                    false,
                    &body("b", &["r1", "r2"], "y"),
                    "2026-01-05T00:00:00Z",
                ),
            ],
        ),
    ]);
    let output = render(&report, false);
    assert!(
        output.contains("same resources exist in a newer file\n            ↳ new/terraform.tfstate @w2 HEAD  4 days ago\n"),
        "{output}"
    );
}

#[test]
fn source_written_after_the_move_is_deprecated_not_obsolete() {
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![
                live(
                    "v2",
                    true,
                    &body("a", &["r1", "r2"], "y"),
                    "2026-01-03T00:00:00Z",
                ),
                live(
                    "v1",
                    false,
                    &body("a", &["r1", "r2"], "x"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
        history(
            "new/terraform.tfstate",
            vec![
                live(
                    "w2",
                    true,
                    &body("b", &["r1", "r2"], "z"),
                    "2026-01-05T00:00:00Z",
                ),
                live(
                    "w1",
                    false,
                    &body("b", &["r1", "r2"], "w"),
                    "2026-01-02T00:00:00Z",
                ),
            ],
        ),
    ]);
    let output = render(&report, false);
    assert!(
        output.contains("  DEPRECATED  old/terraform.tfstate  @v2       2 res  #"),
        "{output}"
    );
    assert!(
        output.contains("same resources exist in a newer file, but written again afterwards"),
        "{output}"
    );
    // The version with the same resources follows on the next line, under the key column.
    assert!(
        output.contains(
            "written again afterwards\n              ↳ new/terraform.tfstate @w2 HEAD  5 days ago\n"
        ),
        "{output}"
    );
    assert!(
        output.ends_with("removal candidates: 1 (1 DEPRECATED)\n"),
        "{output}"
    );
    // old was written again on the 3rd, after new had picked up its resources on the 2nd.
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Deprecated {
            kind: DeprecatedKind::WrittenAfterMove {
                kind: ObsoleteKind::MovedSameIds
            }
        }
    );
    assert_eq!(status(&report, "new/terraform.tfstate"), &KeyStatus::Active);
}

#[test]
fn pure_move_with_same_ids_but_different_lineage_and_outputs() {
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1", "r2"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "new/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r2", "r1"], "y"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Obsolete {
            kind: ObsoleteKind::MovedSameIds
        }
    );
    assert!(!report.edges[0].identical);
}

#[test]
fn split_into_two_keys_is_obsolete_with_partial_edges() {
    let report = report(&split_histories());
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Obsolete {
            kind: ObsoleteKind::Split { into: 2 }
        }
    );
    let counts: Vec<_> = report
        .edges
        .iter()
        .map(|edge| (edge.to.key.as_str(), edge.resources, edge.of))
        .collect();
    assert_eq!(
        counts,
        [("new/app.tfstate", 3, 11), ("new/infra.tfstate", 8, 11)]
    );
    assert_eq!(status(&report, "new/infra.tfstate"), &KeyStatus::Active);
    assert_eq!(status(&report, "new/app.tfstate"), &KeyStatus::Active);
}

#[test]
fn merge_into_a_superset_key_is_obsolete() {
    let report = report(&[
        history(
            "small/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1", "r2"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "big/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r1", "r2", "r3", "r4"], "y"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    assert_eq!(
        status(&report, "small/terraform.tfstate"),
        &KeyStatus::Obsolete {
            kind: ObsoleteKind::Merged {
                into: "big/terraform.tfstate".to_string()
            }
        }
    );
    assert_eq!(status(&report, "big/terraform.tfstate"), &KeyStatus::Active);
}

#[test]
fn partial_branch_leaves_the_source_active() {
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1", "r2", "r3"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "new/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r1"], "y"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    assert_eq!(status(&report, "old/terraform.tfstate"), &KeyStatus::Active);
    assert_eq!(report.edges.len(), 1);
    assert_eq!((report.edges[0].resources, report.edges[0].of), (1, 3));
}

#[test]
fn source_that_kept_resources_after_the_copy_still_branches() {
    let content = body("a", &["r1", "r2"], "x");
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![
                marker("m1", true, "2026-01-04T00:00:00Z"),
                live(
                    "v2",
                    false,
                    &body("a", &["r1", "r2"], "y"),
                    "2026-01-03T00:00:00Z",
                ),
                live("v1", false, &content, "2026-01-01T00:00:00Z"),
            ],
        ),
        history(
            "new/terraform.tfstate",
            vec![
                live(
                    "w2",
                    true,
                    &body("b", &["r1", "r2"], "z"),
                    "2026-01-05T00:00:00Z",
                ),
                live("w1", false, &content, "2026-01-02T00:00:00Z"),
            ],
        ),
    ]);
    assert_eq!(report.edges.len(), 1);
    assert_eq!(report.edges[0].from.key, "old/terraform.tfstate");
    assert_eq!(report.edges[0].to.version_id, "w1");
    assert_eq!((report.edges[0].resources, report.edges[0].of), (2, 2));
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Deleted {
            elsewhere: 2,
            of: 2
        }
    );
}

#[test]
fn deleted_key_partially_covered_reports_counts() {
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![
                marker("m1", true, "2026-01-03T00:00:00Z"),
                live(
                    "v1",
                    false,
                    &body("a", &["r1", "r2", "r3"], "x"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
        history(
            "new/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r1", "r2"], "y"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Deleted {
            elsewhere: 2,
            of: 3
        }
    );
}

#[test]
fn two_live_keys_with_the_same_resources_only_obsolete_the_older_written() {
    let report = report(&[
        history(
            "a/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "b/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r1"], "y"),
                "2026-01-02T00:00:00Z",
            )],
        ),
    ]);
    assert!(matches!(
        status(&report, "a/terraform.tfstate"),
        KeyStatus::Obsolete { .. }
    ));
    assert_eq!(status(&report, "b/terraform.tfstate"), &KeyStatus::Active);
}

#[test]
fn live_key_is_not_obsoleted_by_a_key_written_before_its_latest_write() {
    // b holds newer versions than a's snapshot, but a was written to again afterwards.
    let report = report(&[
        history(
            "a/terraform.tfstate",
            vec![
                live("v2", true, &body("a", &["r1"], "z"), "2026-01-06T00:00:00Z"),
                live(
                    "v1",
                    false,
                    &body("a", &["r1"], "x"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
        history(
            "b/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r1"], "y"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    assert_eq!(status(&report, "a/terraform.tfstate"), &KeyStatus::Active);
}

#[test]
fn empty_state_is_never_obsolete_but_empty() {
    let report = report(&[
        history(
            "a/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &[], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "b/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &[], "x"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    assert_eq!(status(&report, "a/terraform.tfstate"), &KeyStatus::Empty);
    assert_eq!(status(&report, "b/terraform.tfstate"), &KeyStatus::Empty);
    assert!(report.edges.is_empty());
    assert_eq!(
        render(&report, false),
        "\
standalone  (2 state files, 0 to keep)

  EMPTY  b/terraform.tfstate  @w1  0 res  5 days ago  no resources left
  EMPTY  a/terraform.tfstate  @v1  0 res  9 days ago  no resources left

removal candidates: 2 (2 EMPTY)
"
    );
}

#[test]
fn emptied_state_with_branches_is_empty_and_deleted_empty_state_stays_deleted() {
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![
                live("v2", true, &body("a", &[], "x"), "2026-01-03T00:00:00Z"),
                live(
                    "v1",
                    false,
                    &body("a", &["r1"], "x"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
        history(
            "new/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r1"], "y"),
                "2026-01-02T00:00:00Z",
            )],
        ),
        history(
            "gone/terraform.tfstate",
            vec![
                marker("m1", true, "2026-01-04T00:00:00Z"),
                live("g1", false, &body("c", &[], "z"), "2026-01-01T00:00:00Z"),
            ],
        ),
    ]);
    assert_eq!(status(&report, "old/terraform.tfstate"), &KeyStatus::Empty);
    assert_eq!(report.edges.len(), 1);
    assert_eq!(
        status(&report, "gone/terraform.tfstate"),
        &KeyStatus::Deleted {
            elsewhere: 0,
            of: 0
        }
    );
}

#[test]
fn arn_identifies_resources_without_an_id() {
    let arn_state = |lineage: &str| {
        serde_json::json!({
            "version": 4,
            "lineage": lineage,
            "resources": [{
                "mode": "managed", "type": "t", "name": "n",
                "instances": [{"attributes": {"arn": "arn:aws:t:1"}}]
            }]
        })
        .to_string()
    };
    let report = report(&[
        history(
            "old/terraform.tfstate",
            vec![live("v1", true, &arn_state("a"), "2026-01-01T00:00:00Z")],
        ),
        history(
            "new/terraform.tfstate",
            vec![live("w1", true, &arn_state("b"), "2026-01-05T00:00:00Z")],
        ),
    ]);
    assert_eq!(
        status(&report, "old/terraform.tfstate"),
        &KeyStatus::Obsolete {
            kind: ObsoleteKind::MovedSameIds
        }
    );
    assert_eq!(report.edges.len(), 1);
}

fn cycle_histories() -> Vec<KeyHistory> {
    // r1 moves a -> b while r2 moves b -> a, so the edges form a cycle.
    vec![
        history(
            "a/terraform.tfstate",
            vec![
                live("v2", true, &body("a", &["r2"], "x"), "2026-01-03T00:00:00Z"),
                live(
                    "v1",
                    false,
                    &body("a", &["r1"], "x"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
        history(
            "b/terraform.tfstate",
            vec![
                live("w2", true, &body("b", &["r1"], "y"), "2026-01-02T00:00:00Z"),
                live(
                    "w1",
                    false,
                    &body("b", &["r2"], "y"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
    ]
}

#[test]
fn cycle_renders_without_looping() {
    let report = report(&cycle_histories());
    assert_eq!(report.edges.len(), 2);
    let output = render(&report, false);
    let (a, b) = (h(&["r2"]), h(&["r1"]));
    assert_eq!(
        output,
        format!(
            "\
a  (2 state files, 2 to keep)

  ACTIVE  a/terraform.tfstate  @v2 HEAD  1 res  #{a}  7 days ago
  ACTIVE  b/terraform.tfstate  @w2 HEAD  1 res  #{b}  8 days ago

  history
    8 days ago  a  →  b  1 resource  (source had 1)
    7 days ago  b  →  a  1 resource  (source had 1)

removal candidates: none
"
        )
    );
}

#[test]
fn group_rendering_snapshot() {
    let report = report(&split_histories());
    let (old, app, infra) = (h(&IDS), h(&IDS[8..]), h(&IDS[..8]));
    assert_eq!(
        render(&report, false),
        format!(
            "\
app  (3 state files, 2 to keep)

  ACTIVE    new/app.tfstate        @x1 HEAD   3 res  #{app}  2 days ago
  ACTIVE    new/infra.tfstate      @w1 HEAD   8 res  #{infra}  5 days ago
  OBSOLETE  old/terraform.tfstate  @v1       11 res  #{old}  9 days ago  resources split into 2 other files

  history
    5 days ago  old  →  new/infra  8 resources  (source had 11)
    2 days ago  old  →  new/app    3 resources  (source had 11)

removal candidates: 1 (1 OBSOLETE)
"
        )
    );
}

#[test]
fn verbose_rendering_snapshot() {
    let report = report(&split_histories());
    let (old, app, infra) = (h(&IDS), h(&IDS[8..]), h(&IDS[..8]));
    assert_eq!(
        render(&report, true),
        format!(
            "\
app  (3 state files, 2 to keep)

  ACTIVE    new/app.tfstate        @x1 HEAD   3 res  #{app}  2 days ago
      s3://b/new/app.tfstate @x1 #{app} CURRENT 2 days ago
  ACTIVE    new/infra.tfstate      @w1 HEAD   8 res  #{infra}  5 days ago
      s3://b/new/infra.tfstate @w1 #{infra} CURRENT 5 days ago
  OBSOLETE  old/terraform.tfstate  @v1       11 res  #{old}  9 days ago  resources split into 2 other files
      s3://b/old/terraform.tfstate @v1 #{old} CURRENT 9 days ago

  history
    5 days ago  old  →  new/infra  8 resources  (source had 11)  @v1 → @w1
    2 days ago  old  →  new/app    3 resources  (source had 11)  @v1 → @x1

removal candidates: 1 (1 OBSOLETE)
"
        )
    );
}

#[test]
fn resource_hash_is_independent_of_order_and_full_hash_is_untruncated() {
    let report = report(&[
        history(
            "a/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1", "r2"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "b/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r2", "r1"], "y"),
                "2026-01-01T00:00:00Z",
            )],
        ),
    ]);
    let hashes: Vec<_> = report
        .keys
        .iter()
        .map(|node| node.resource_hash.clone().unwrap())
        .collect();
    assert_eq!(hashes[0], hashes[1]);
    assert_eq!(hashes[0].len(), 64);
    let full = format_terminal(&report, false, false, true, now());
    assert!(full.contains(&format!("#{}", hashes[0])), "{full}");
    let short = render(&report, false);
    assert!(!short.contains(&hashes[0]), "{short}");
    assert!(
        short.contains(&format!("#{} ", &hashes[0][..16])),
        "{short}"
    );
}

#[test]
fn merge_shows_both_sources_in_one_group() {
    let report = report(&[
        history(
            "a/terraform.tfstate",
            vec![live(
                "v1",
                true,
                &body("a", &["r1"], "x"),
                "2026-01-01T00:00:00Z",
            )],
        ),
        history(
            "b/terraform.tfstate",
            vec![live(
                "w1",
                true,
                &body("b", &["r2", "r3"], "x"),
                "2026-01-02T00:00:00Z",
            )],
        ),
        history(
            "c/terraform.tfstate",
            vec![live(
                "x1",
                true,
                &body("c", &["r1", "r2", "r3"], "x"),
                "2026-01-05T00:00:00Z",
            )],
        ),
    ]);
    let (a, b, c) = (h(&["r1"]), h(&["r2", "r3"]), h(&["r1", "r2", "r3"]));
    assert_eq!(
        render(&report, false),
        format!(
            "\
c  (3 state files, 1 to keep)

  ACTIVE    c/terraform.tfstate  @x1 HEAD  3 res  #{c}  5 days ago
  OBSOLETE  b/terraform.tfstate  @w1       2 res  #{b}  8 days ago  all resources merged into c/terraform.tfstate
  OBSOLETE  a/terraform.tfstate  @v1       1 res  #{a}  9 days ago  all resources merged into c/terraform.tfstate

  history
    5 days ago  a  →  c  1 resource   (source had 1)
    5 days ago  b  →  c  2 resources  (source had 2)

removal candidates: 2 (2 OBSOLETE)
"
        )
    );
}

#[test]
fn output_never_contains_resource_ids_or_attributes() {
    let report = report(&split_histories());
    for output in [
        render(&report, true),
        format_terminal(&report, true, true, true, now()),
        format_json(&report),
    ] {
        assert!(!output.contains("attribute-content"), "{output}");
        assert!(!output.contains("r10"), "{output}");
    }
}

#[test]
fn json_lists_statuses_and_edges() {
    let report = report(&split_histories());
    let value: serde_json::Value = serde_json::from_str(&format_json(&report)).unwrap();
    assert_eq!(value["bucket"], "b");
    let old = value["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["key"] == "old/terraform.tfstate")
        .unwrap();
    assert_eq!(old["status"], "OBSOLETE");
    assert_eq!(old["detail"], "split into 2 states");
    assert_eq!(value["edges"].as_array().unwrap().len(), 2);
}

#[test]
fn groups_shorten_keys_in_the_history_and_list_standalone_files_last() {
    let report = report(&[
        history(
            "dev/ec2-instance/terraform.tfstate",
            vec![
                live("e2", true, &body("a", &[], "x"), "2026-01-03T00:00:00Z"),
                live(
                    "e1",
                    false,
                    &body("a", &["r1", "r2"], "x"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
        ),
        history(
            "dev/euc1/acme/infrastructure/testing/ingress-test-instance.tfstate",
            vec![live(
                "t1",
                true,
                &body("b", &["r1"], "y"),
                "2026-01-02T00:00:00Z",
            )],
        ),
        history(
            "dev/unrelated/terraform.tfstate",
            vec![live(
                "u1",
                true,
                &body("c", &["r9"], "z"),
                "2026-01-04T00:00:00Z",
            )],
        ),
    ]);
    let (r1, r9) = (h(&["r1"]), h(&["r9"]));
    assert_eq!(
        render(&report, false),
        format!(
            "\
ingress-test-instance  (2 state files, 1 to keep)

  ACTIVE  dev/euc1/acme/infrastructure/testing/ingress-test-instance.tfstate  @t1 HEAD  1 res  #{r1}  8 days ago
  EMPTY   dev/ec2-instance/terraform.tfstate                                  @e2       0 res                     7 days ago  no resources left

  history
    8 days ago  dev/ec2-instance  →  …/testing/ingress-test-instance  1 resource  (source had 2)

standalone  (1 state file, 1 to keep)

  ACTIVE  dev/unrelated/terraform.tfstate  @u1 HEAD  1 res  #{r9}  6 days ago

removal candidates: 1 (1 EMPTY)
"
        )
    );
}
