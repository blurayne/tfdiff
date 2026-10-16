//! Detection of resources that moved from one state file to another.
//!
//! In an S3 history report a move shows up as a `DESTROY` in one state file and a `CREATE`
//! of the same resource type with the same `id`, `arn` or `name` attribute in another state
//! file. Each create is paired with the closest-in-time unused destroy; the attribute
//! differences between the two snapshots are kept so a move that also modified the resource
//! is visible.

use crate::inventory::{BatchEvent, BatchEventKind, BatchReport};
use crate::{ActionType, Change, Resource};
use colored::*;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// Identity attributes in matching order.
const IDENTITY_KEYS: [&str; 3] = ["id", "arn", "name"];

#[derive(Clone, Debug, Serialize)]
pub struct StateMigration {
    pub type_name: String,
    pub identity_key: String,
    pub identity_value: String,
    pub from: MigrationEnd,
    pub to: MigrationEnd,
    /// Attribute differences between the destroyed and the created snapshot.
    pub changes: Vec<Change>,
}

/// One side of a move: the state file and address the resource left or arrived at.
#[derive(Clone, Debug, Serialize)]
pub struct MigrationEnd {
    pub key: String,
    pub uri: String,
    pub address: String,
    pub timestamp: String,
    pub version_id: Option<String>,
}

#[derive(Serialize)]
struct MigrationReport<'a> {
    source: &'a str,
    complete: bool,
    moved_resources: usize,
    migrations: &'a [StateMigration],
    errors: &'a [String],
}

struct Candidate<'a> {
    event: &'a BatchEvent,
    resource: &'a Resource,
    identity: (&'static str, String),
}

/// The attribute identity used for matching; placeholders and unknown values never match.
fn attribute_identity(resource: &Resource) -> Option<(&'static str, String)> {
    IDENTITY_KEYS.into_iter().find_map(|key| {
        let value = match resource.attributes.get(key)? {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => return None,
        };
        let usable = !value.is_empty()
            && value != crate::secrets::SENSITIVE_PLACEHOLDER
            && value != "(known after apply)";
        usable.then_some((key, value))
    })
}

fn bucket_of(source: &str) -> &str {
    source
        .strip_prefix("s3://")
        .map(|rest| rest.split_once('/').map_or(rest, |(bucket, _)| bucket))
        .unwrap_or_default()
}

fn event_seconds(event: &BatchEvent) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&event.timestamp)
        .ok()
        .map(|time| time.timestamp())
}

fn same_event(end: &MigrationEnd, event: &BatchEvent) -> bool {
    end.key == event.key && end.timestamp == event.timestamp && end.version_id == event.version_id
}

fn end_of(bucket: &str, candidate: &Candidate) -> MigrationEnd {
    MigrationEnd {
        key: candidate.event.key.clone(),
        uri: format!("s3://{bucket}/{}", candidate.event.key),
        address: candidate.resource.id.clone(),
        timestamp: candidate.event.timestamp.clone(),
        version_id: candidate.event.version_id.clone(),
    }
}

/// Pairs destroys and creates of the same resource across different state files.
pub fn detect_migrations(report: &BatchReport) -> Vec<StateMigration> {
    let bucket = bucket_of(&report.source);
    let mut destroys = Vec::new();
    let mut creates = Vec::new();
    for event in &report.events {
        let (BatchEventKind::Diff, Some(plan)) = (&event.kind, &event.plan) else {
            continue;
        };
        for resource in &plan.resources {
            let target = match resource.action {
                ActionType::Destroy => &mut destroys,
                ActionType::Create => &mut creates,
                _ => continue,
            };
            if let Some(identity) = attribute_identity(resource) {
                target.push(Candidate {
                    event,
                    resource,
                    identity,
                });
            }
        }
    }

    let mut used = vec![false; destroys.len()];
    let mut migrations = Vec::new();
    for create in &creates {
        let create_time = event_seconds(create.event);
        let best = destroys
            .iter()
            .enumerate()
            .filter(|(index, destroy)| {
                !used[*index]
                    && destroy.event.key != create.event.key
                    && destroy.resource.type_name == create.resource.type_name
                    && destroy.identity == create.identity
            })
            .min_by_key(
                |(_, destroy)| match (event_seconds(destroy.event), create_time) {
                    (Some(left), Some(right)) => (left - right).unsigned_abs(),
                    _ => u64::MAX,
                },
            );
        let Some((index, destroy)) = best else {
            continue;
        };
        used[index] = true;
        migrations.push(StateMigration {
            type_name: create.resource.type_name.clone(),
            identity_key: create.identity.0.to_string(),
            identity_value: create.identity.1.clone(),
            from: end_of(bucket, destroy),
            to: end_of(bucket, create),
            changes: crate::diff_attribute_maps(
                &destroy.resource.attributes,
                &create.resource.attributes,
            ),
        });
    }
    migrations.sort_by(|left, right| {
        (&left.type_name, &left.identity_value, &left.from.timestamp).cmp(&(
            &right.type_name,
            &right.identity_value,
            &right.from.timestamp,
        ))
    });
    migrations
}

fn change_note(migration: &StateMigration) -> String {
    match migration.changes.len() {
        0 => String::new(),
        1 => ", 1 attribute changed".to_string(),
        count => format!(", {count} attributes changed"),
    }
}

/// Resource address → move note for the resources of one event.
pub fn moves_for_event(
    migrations: &[StateMigration],
    event: &BatchEvent,
) -> HashMap<String, String> {
    let mut moves = HashMap::new();
    for migration in migrations {
        if same_event(&migration.from, event) {
            moves.insert(
                migration.from.address.clone(),
                format!(
                    "moved to {} ({}){}",
                    migration.to.uri,
                    migration.to.address,
                    change_note(migration)
                ),
            );
        }
        if same_event(&migration.to, event) {
            moves.insert(
                migration.to.address.clone(),
                format!(
                    "moved from {} ({}){}",
                    migration.from.uri,
                    migration.from.address,
                    change_note(migration)
                ),
            );
        }
    }
    moves
}

fn version_suffix(end: &MigrationEnd) -> String {
    end.version_id
        .as_deref()
        .map(|version| format!(" ({version})"))
        .unwrap_or_default()
}

fn summary_line(report: &BatchReport) -> String {
    format!(
        "State migrations: {} moved resource{} across {} matching keys{}",
        report.migrations.len(),
        if report.migrations.len() == 1 {
            ""
        } else {
            "s"
        },
        report.counts.matching_keys,
        if report.complete {
            ""
        } else {
            " | INCOMPLETE INVENTORY"
        }
    )
}

pub fn format_migrations_summary(report: &BatchReport) -> String {
    format!("{}\n", summary_line(report))
}

/// Terminal view of `--only-state-migrations`: one block per move, sorted by resource.
pub fn format_migrations_terminal(report: &BatchReport, color: bool) -> String {
    use crate::formatter::sanitize_terminal;
    colored::control::set_override(color);
    let mut output = format!("{}\n", summary_line(report).bold());
    if report.migrations.is_empty() {
        output.push_str("\nNo resources moved between state files.\n");
    }
    for migration in &report.migrations {
        output.push_str(&format!(
            "\n{} {}  {} {}\n",
            " MOVED ".on_yellow().black().bold(),
            sanitize_terminal(&migration.type_name).bright_blue(),
            format!("{}:", migration.identity_key).dimmed(),
            sanitize_terminal(&migration.identity_value)
        ));
        for (label, end) in [
            ("FROM".bright_red().bold(), &migration.from),
            ("TO  ".bright_green().bold(), &migration.to),
        ] {
            output.push_str(&format!(
                "  {} {}  {}  {}\n",
                label,
                sanitize_terminal(&end.uri),
                sanitize_terminal(&end.address).bright_white(),
                sanitize_terminal(&format!("{}{}", end.timestamp, version_suffix(end))).dimmed()
            ));
        }
        if migration.changes.is_empty() {
            output.push_str(&format!("  {}\n", "unchanged during move".dimmed()));
        } else {
            output.push_str(&format!(
                "  {}\n",
                format!("modified during move{}:", change_note(migration)).yellow()
            ));
            output.push_str(&crate::formatter::terminal::format_changes(
                &migration.changes,
                &ActionType::Update,
            ));
        }
    }
    if !report.errors.is_empty() {
        output.push_str("\nErrors:\n");
        for error in &report.errors {
            output.push_str(&format!("- {}\n", sanitize_terminal(error)));
        }
    }
    output
}

pub fn format_migrations_markdown(report: &BatchReport) -> String {
    use crate::formatter::markdown_code_span;
    let mut md = format!(
        "# S3 State Migrations\n\nSource: {}  \n{}\n",
        markdown_code_span(&report.source),
        summary_line(report)
    );
    if report.migrations.is_empty() {
        md.push_str("\nNo resources moved between state files.\n");
    }
    for migration in &report.migrations {
        md.push_str(&format!(
            "\n## 🔀 MOVED {} {}\n\n| | State file | Address | Time |\n|---|---|---|---|\n",
            markdown_code_span(&migration.type_name),
            markdown_code_span(&format!(
                "{}: {}",
                migration.identity_key, migration.identity_value
            ))
        ));
        for (label, end) in [("From", &migration.from), ("To", &migration.to)] {
            md.push_str(&format!(
                "| {label} | {} | {} | {}{} |\n",
                markdown_code_span(&end.uri),
                markdown_code_span(&end.address),
                end.timestamp,
                version_suffix(end)
            ));
        }
        if migration.changes.is_empty() {
            md.push_str("\nUnchanged during move.\n");
        } else {
            md.push_str(&format!(
                "\nModified during move{}:\n\n",
                change_note(migration)
            ));
            md.push_str(&crate::formatter::markdown::markdown_changes_table(
                &migration.changes,
            ));
        }
    }
    if !report.errors.is_empty() {
        md.push_str("\n## Errors\n");
        for error in &report.errors {
            md.push_str(&format!("\n- {error}"));
        }
        md.push('\n');
    }
    md
}

pub fn format_migrations_html(report: &BatchReport) -> String {
    use crate::formatter::html::html_escape;
    let mut html = crate::formatter::html::html_document_start("S3 state migrations");
    html.push_str(&format!(
        "<h1>S3 state migrations</h1><div class=\"batch-summary\">Source <code>{}</code> · {}</div>",
        html_escape(&report.source),
        html_escape(&summary_line(report))
    ));
    if report.migrations.is_empty() {
        html.push_str("<p>No resources moved between state files.</p>");
    }
    for migration in &report.migrations {
        html.push_str(&format!(
            "<section class=\"resource-section\"><header class=\"resource-header\"><span class=\"action-badge action-moved\">MOVED</span><h2 class=\"resource-name\">{}</h2><span class=\"muted\">{}: {}</span></header><div class=\"diff-container\"><table><thead><tr><th scope=\"col\"></th><th scope=\"col\">State file</th><th scope=\"col\">Address</th><th scope=\"col\">Time</th></tr></thead><tbody>",
            html_escape(&migration.type_name),
            html_escape(&migration.identity_key),
            html_escape(&migration.identity_value)
        ));
        for (label, class, end) in [
            ("From", "row-remove", &migration.from),
            ("To", "row-add", &migration.to),
        ] {
            html.push_str(&format!(
                "<tr class=\"{class}\"><td class=\"path\">{label}</td><td class=\"value\">{}</td><td class=\"value\">{}</td><td class=\"value\">{}</td></tr>",
                html_escape(&end.uri),
                html_escape(&end.address),
                html_escape(&format!("{}{}", end.timestamp, version_suffix(end)))
            ));
        }
        html.push_str("</tbody></table></div>");
        if migration.changes.is_empty() {
            html.push_str("<div class=\"resource-source\">Unchanged during move.</div>");
        } else {
            html.push_str(&format!(
                "<div class=\"resource-source moved-note\">Modified during move{}:</div>",
                html_escape(&change_note(migration))
            ));
            html.push_str(&crate::formatter::html::render_changes(
                &migration.changes,
                &crate::PlanMode::StateDiff,
            ));
        }
        html.push_str("</section>");
    }
    if !report.errors.is_empty() {
        html.push_str("<h2 class=\"batch-error\">Errors</h2><ul>");
        for error in &report.errors {
            html.push_str(&format!("<li>{}</li>", html_escape(error)));
        }
        html.push_str("</ul>");
    }
    html.push_str("</main></body></html>");
    html
}

pub fn format_migrations_json(report: &BatchReport) -> crate::Result<String> {
    serde_json::to_string_pretty(&MigrationReport {
        source: &report.source,
        complete: report.complete,
        moved_resources: report.migrations.len(),
        migrations: &report.migrations,
        errors: &report.errors,
    })
    .map_err(crate::TfdiffError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::BatchCounts;
    use crate::{PlanMode, TerraformPlan};
    use serde_json::json;

    fn resource(address: &str, action: ActionType, attributes: Value) -> Resource {
        let (type_name, name) = address.rsplit_once('.').unwrap();
        let type_name = type_name.rsplit('.').next().unwrap();
        Resource {
            id: address.to_string(),
            plan_delta: None,
            name: name.to_string(),
            type_name: type_name.to_string(),
            provider: String::new(),
            action,
            changes: Vec::new(),
            attributes: serde_json::from_value(attributes).unwrap(),
            audit_attribution: None,
            applied: true,
            source: None,
        }
    }

    fn event(key: &str, timestamp: &str, version: &str, resources: Vec<Resource>) -> BatchEvent {
        let plan = TerraformPlan {
            mode: PlanMode::StateDiff,
            resources,
            ..Default::default()
        };
        BatchEvent::test_diff(key, timestamp, version, plan)
    }

    fn report(events: Vec<BatchEvent>) -> BatchReport {
        let mut report = BatchReport {
            source: "s3://acme-states/env/".to_string(),
            complete: true,
            counts: BatchCounts {
                matching_keys: 3,
                ..Default::default()
            },
            events,
            inventory: None,
            errors: Vec::new(),
            skip_deleted_note: None,
            migrations: Vec::new(),
        };
        report.migrations = detect_migrations(&report);
        report
    }

    fn sample() -> BatchReport {
        report(vec![
            event(
                "env/a.tfstate",
                "2026-01-01T10:00:00Z",
                "va1",
                vec![
                    resource(
                        "aws_iam_role.app",
                        ActionType::Destroy,
                        json!({"id": "acme-app", "arn": "arn:aws:iam::000000000000:role/acme-app", "max_session_duration": 3600}),
                    ),
                    resource(
                        "aws_s3_bucket.logs",
                        ActionType::Destroy,
                        json!({"id": "acme-logs"}),
                    ),
                ],
            ),
            event(
                "env/b.tfstate",
                "2026-01-01T10:05:00Z",
                "vb1",
                vec![
                    resource(
                        "module.iam.aws_iam_role.app",
                        ActionType::Create,
                        json!({"id": "acme-app", "arn": "arn:aws:iam::000000000000:role/acme-app", "max_session_duration": 7200}),
                    ),
                    resource(
                        "aws_s3_bucket.other",
                        ActionType::Create,
                        json!({"id": "acme-other"}),
                    ),
                ],
            ),
            event(
                "env/c.tfstate",
                "2026-01-02T08:00:00Z",
                "vc1",
                vec![resource(
                    "aws_s3_bucket.logs",
                    ActionType::Create,
                    json!({"id": "acme-logs"}),
                )],
            ),
        ])
    }

    #[test]
    fn moves_are_paired_across_files_and_sorted_by_resource() {
        let report = sample();
        let moved: Vec<_> = report
            .migrations
            .iter()
            .map(|m| {
                (
                    m.type_name.as_str(),
                    m.identity_value.as_str(),
                    m.from.uri.as_str(),
                    m.to.address.as_str(),
                    m.changes.len(),
                )
            })
            .collect();
        assert_eq!(
            moved,
            [
                (
                    "aws_iam_role",
                    "acme-app",
                    "s3://acme-states/env/a.tfstate",
                    "module.iam.aws_iam_role.app",
                    1
                ),
                (
                    "aws_s3_bucket",
                    "acme-logs",
                    "s3://acme-states/env/a.tfstate",
                    "aws_s3_bucket.logs",
                    0
                ),
            ]
        );
        assert_eq!(
            report.migrations[0].changes[0].path,
            ["max_session_duration"]
        );
    }

    #[test]
    fn same_file_replacements_and_placeholders_are_not_moves() {
        let report = report(vec![
            event(
                "env/a.tfstate",
                "2026-01-01T10:00:00Z",
                "va1",
                vec![
                    resource(
                        "aws_instance.web",
                        ActionType::Destroy,
                        json!({"id": "i-1"}),
                    ),
                    resource(
                        "aws_instance.web2",
                        ActionType::Create,
                        json!({"id": "i-1"}),
                    ),
                    resource(
                        "aws_ssm_parameter.p",
                        ActionType::Destroy,
                        json!({"id": crate::secrets::SENSITIVE_PLACEHOLDER}),
                    ),
                ],
            ),
            event(
                "env/b.tfstate",
                "2026-01-01T11:00:00Z",
                "vb1",
                vec![
                    resource(
                        "aws_ssm_parameter.p",
                        ActionType::Create,
                        json!({"id": crate::secrets::SENSITIVE_PLACEHOLDER}),
                    ),
                    resource("aws_eip.web", ActionType::Create, json!({"id": "i-1"})),
                ],
            ),
        ]);
        assert!(report.migrations.is_empty());
    }

    #[test]
    fn event_annotations_point_to_the_other_side() {
        let report = sample();
        let from = moves_for_event(&report.migrations, &report.events[0]);
        assert_eq!(
            from["aws_iam_role.app"],
            "moved to s3://acme-states/env/b.tfstate (module.iam.aws_iam_role.app), 1 attribute changed"
        );
        let to = moves_for_event(&report.migrations, &report.events[2]);
        assert_eq!(
            to["aws_s3_bucket.logs"],
            "moved from s3://acme-states/env/a.tfstate (aws_s3_bucket.logs)"
        );
        assert!(!to.contains_key("aws_s3_bucket.other"));
    }

    #[test]
    fn migration_views_show_both_ends() {
        let _lock = crate::TEST_COLOR_LOCK.lock().unwrap();
        let report = sample();
        let terminal = format_migrations_terminal(&report, false);
        assert!(terminal.contains("State migrations: 2 moved resources across 3 matching keys"));
        assert!(terminal.contains(" MOVED  aws_iam_role  id: acme-app"));
        assert!(terminal.contains(
            "FROM s3://acme-states/env/a.tfstate  aws_iam_role.app  2026-01-01T10:00:00Z (va1)"
        ));
        assert!(
            terminal.contains("TO   s3://acme-states/env/b.tfstate  module.iam.aws_iam_role.app")
        );
        assert!(terminal.contains("modified during move, 1 attribute changed:"));
        assert!(terminal.contains("unchanged during move"));

        let markdown = format_migrations_markdown(&report);
        assert!(markdown.contains("## 🔀 MOVED `aws_iam_role` `id: acme-app`"));
        assert!(markdown.contains("| `max_session_duration` | 3600 | 7200 |"));

        let html = format_migrations_html(&report);
        assert!(html.contains("action-moved"));
        assert!(html.contains("module.iam.aws_iam_role.app"));

        let json: Value = serde_json::from_str(&format_migrations_json(&report).unwrap()).unwrap();
        assert_eq!(json["moved_resources"], 2);
        assert_eq!(json["migrations"][0]["to"]["key"], "env/b.tfstate");
    }
}
