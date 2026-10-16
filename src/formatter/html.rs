use crate::{ActionType, PlanMode, TerraformPlan, WarningLevel};
use chrono::Local;
use serde_json::Value;

pub fn format_html_output(plan: &TerraformPlan) -> String {
    format_html_output_with_options(plan, super::FormatterOptions::default())
}

pub fn format_html_output_with_options(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
) -> String {
    format!(
        "{}{}\n</main>\n</body>\n</html>",
        html_document_start("Terraform changes"),
        render_html_fragment_with_options(plan, options)
    )
}

pub fn render_html_fragment(plan: &TerraformPlan) -> String {
    render_html_fragment_with_options(plan, super::FormatterOptions::default())
}

pub fn render_html_fragment_with_options(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
) -> String {
    render_html_fragment_with_moves(plan, options, None)
}

/// Like [`render_html_fragment_with_options`], but resources whose address is a key in
/// `moves` get a `MOVED` badge and the map value as a note.
pub fn render_html_fragment_with_moves(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
    moves: Option<&std::collections::HashMap<String, String>>,
) -> String {
    let mut html = String::new();

    let (title, subtitle) = match plan.mode {
        PlanMode::StateDiff => (
            "Terraform State Snapshot Diff",
            "Observed differences between two state snapshots; actions are not predictions.",
        ),
        PlanMode::PlanDiff => (
            "Terraform Plan Diff",
            "Differences between two Terraform plan reports.",
        ),
        _ => ("Terraform Plan Analysis", "Infrastructure changes"),
    };
    let timestamp = Local::now().format("%B %d, %Y").to_string();
    html.push_str(&format!(
        "<header><h1>{}</h1><p class=\"subtitle\">{}</p><p class=\"timestamp\">Generated on {}</p></header>",
        html_escape(title),
        html_escape(subtitle),
        html_escape(&timestamp)
    ));

    if let Some(attribution) = &plan.author_attribution {
        html.push_str("<section class=\"audit-attribution\" aria-label=\"CloudTrail attribution\"><h2>Observed resource actors</h2>");
        html.push_str(&format!(
            "<p>Audit window: <code>{}</code>–<code>{}</code> | Regions: {} | Status: {}</p>",
            html_escape(&attribution.window_start),
            html_escape(&attribution.window_end),
            html_escape(&attribution.regions.join(", ")),
            html_escape(&format!("{:?}", attribution.status)),
        ));
        if let Some(message) = &attribution.message {
            html.push_str(&format!("<p>{}</p>", html_escape(message)));
        }
        if !attribution.observations.is_empty() {
            html.push_str("<ul>");
            for observation in &attribution.observations {
                let principal = observation
                    .principal_arn
                    .as_deref()
                    .or(observation.principal_id.as_deref())
                    .unwrap_or("unknown principal");
                html.push_str(&format!(
                    "<li><code>{}</code> — {} <code>{}</code> at <time>{}</time> ({})</li>",
                    html_escape(principal),
                    html_escape(&observation.event_name),
                    html_escape(&observation.event_id),
                    html_escape(&observation.event_time),
                    html_escape(&observation.match_basis),
                ));
                if let Some(agent) = &observation.user_agent {
                    html.push_str(&format!(
                        "<p>Event userAgent: <code>{}</code></p>",
                        html_escape(agent)
                    ));
                }
            }
            html.push_str("</ul>");
        }
        html.push_str("</section>");
    }

    if let Some(versions) = &plan.source_versions {
        html.push_str("<section class=\"source-versions\" aria-label=\"Source versions\">");
        for (label, version) in [
            (&versions.left_label, versions.left.as_ref()),
            (&versions.right_label, versions.right.as_ref()),
        ] {
            if let Some(version) = version {
                html.push_str(&format!(
                    "<div class=\"source-version\"><strong>{}</strong><code>{}</code>{}</div>",
                    html_escape(label),
                    html_escape(version.version_id.as_deref().unwrap_or("unknown")),
                    version
                        .last_modified
                        .as_deref()
                        .map(|timestamp| format!("<time>{}</time>", html_escape(timestamp)))
                        .unwrap_or_default(),
                ));
            }
        }
        html.push_str("</section>");
    }

    let summary_title = match plan.mode {
        PlanMode::StateDiff => "Observed Differences",
        PlanMode::PlanDiff => "Plan Report Differences",
        _ => "Plan Summary",
    };
    html.push_str(&format!(
        "<section class=\"summary\" aria-label=\"{}\">",
        summary_title
    ));
    let (add_label, change_label, destroy_label) = match plan.mode {
        PlanMode::StateDiff => ("added", "changed", "removed"),
        PlanMode::PlanDiff => (
            "added to report",
            "changed in report",
            "removed from report",
        ),
        _ => ("to add", "to change", "to destroy"),
    };
    push_summary_item(&mut html, plan.summary.add, add_label, "add-text");
    push_summary_item(&mut html, plan.summary.change, change_label, "change-text");
    push_summary_item(
        &mut html,
        plan.summary.destroy,
        destroy_label,
        "destroy-text",
    );
    if plan.summary.read > 0 || !plan.data_sources.is_empty() {
        let count = if plan.summary.read > 0 {
            plan.summary.read
        } else {
            plan.data_sources.len()
        };
        let read_label = if plan.mode == PlanMode::StateDiff {
            "read"
        } else {
            "to read"
        };
        push_summary_item(&mut html, count, read_label, "read-text");
    }
    html.push_str("</section>");

    for resource in &plan.resources {
        let (action_class, action_text) = action_label(&resource.action);
        let (action_class, action_text) = match (&plan.mode, &resource.plan_delta) {
            (PlanMode::PlanDiff, Some(delta)) => match delta {
                crate::PlanDelta::Added => ("action-create", "ADDED TO REPORT"),
                crate::PlanDelta::Changed => ("action-update", "CHANGED IN REPORT"),
                crate::PlanDelta::Removed => ("action-destroy", "REMOVED FROM REPORT"),
            },
            _ => (action_class, action_text),
        };
        html.push_str(&format!(
            "<section class=\"resource-section\"><header class=\"resource-header\"><span class=\"action-badge {}\">{}</span><h2 class=\"resource-name\">{}</h2>",
            action_class,
            action_text,
            html_escape(&resource.id)
        ));
        let moved = moves.and_then(|moves| moves.get(&resource.id));
        if moved.is_some() {
            html.push_str("<span class=\"action-badge action-moved\">MOVED</span>");
        }
        if options.show_resource_identity
            && matches!(
                resource.action,
                ActionType::Create | ActionType::Update | ActionType::Destroy
            )
        {
            if let Some((key, value)) = super::resource_identity(resource) {
                html.push_str(&format!(
                    "<span class=\"muted\">{}: {}</span>",
                    html_escape(key),
                    html_escape(&value)
                ));
            }
        }
        html.push_str("</header>");
        if let Some(note) = moved {
            html.push_str(&format!(
                "<div class=\"resource-source moved-note\">{}</div>",
                html_escape(note)
            ));
        }
        if let Some(source) = &resource.source {
            html.push_str(&format!(
                "<div class=\"resource-source\">Source: <code>{}</code></div>",
                html_escape(source)
            ));
        }

        if let Some(attribution) = &resource.audit_attribution {
            if !attribution.observations.is_empty() {
                html.push_str(&format!(
                    "<section class=\"resource-audit-attribution\" aria-label=\"Resource audit attribution\"><h3>Audit attribution: {}</h3><ul>",
                    html_escape(&format!("{:?}", attribution.status))
                ));
                for observation in &attribution.observations {
                    let principal = observation
                        .principal_arn
                        .as_deref()
                        .or(observation.principal_id.as_deref())
                        .unwrap_or("unknown principal");
                    html.push_str(&format!(
                        "<li><code>{}</code> — {} <code>{}</code> at <time>{}</time> (match basis: {})",
                        html_escape(principal),
                        html_escape(&observation.event_name),
                        html_escape(&observation.event_id),
                        html_escape(&observation.event_time),
                        html_escape(&observation.match_basis),
                    ));
                    if let Some(session_label) = &observation.session_label {
                        html.push_str(&format!(
                            "<p>Session: <code>{}</code></p>",
                            html_escape(session_label)
                        ));
                    }
                    if let Some(client_hint) = &observation.client_hint {
                        html.push_str(&format!(
                            "<p>Client: <code>{}</code></p>",
                            html_escape(client_hint)
                        ));
                    }
                    html.push_str("</li>");
                }
                html.push_str("</ul></section>");
            }
        }

        if !resource.changes.is_empty() {
            html.push_str(&render_changes(&resource.changes, &plan.mode));
        } else if resource.action == ActionType::Update {
            html.push_str(&render_fallback_diff(resource, &plan.mode));
        } else {
            let side = match resource.action {
                ActionType::Destroy => ChangeSide::Before,
                _ => ChangeSide::After,
            };
            html.push_str(&render_attributes(&resource.attributes, side));
        }
        html.push_str("</section>");
    }

    for data_source in &plan.data_sources {
        html.push_str(&format!(
            "<section class=\"resource-section\"><header class=\"resource-header\"><span class=\"action-badge action-read\">READ</span><h2 class=\"resource-name\">{}</h2></header>",
            html_escape(&data_source.id)
        ));
        html.push_str(&render_attributes(
            &data_source.attributes,
            ChangeSide::After,
        ));
        html.push_str("</section>");
    }

    if !plan.warnings.is_empty() {
        html.push_str("<section class=\"warnings\"><h2>Warnings</h2>");
        for warning in &plan.warnings {
            let level = match &warning.level {
                WarningLevel::Info => "Info",
                WarningLevel::Warning => "Warning",
                WarningLevel::Error => "Error",
            };
            html.push_str(&format!(
                "<p class=\"warning\"><strong>{}:</strong> {}</p>",
                level,
                html_escape(&warning.message)
            ));
        }
        html.push_str("</section>");
    }
    html
}

pub(crate) fn html_document_start(title: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{}</title>
    <style>
        :root {{ color-scheme: light; font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; color: #24292f; background: #fff; }}
        * {{ box-sizing: border-box; }}
        body {{ margin: 0; line-height: 1.5; }}
        main {{ max-width: 1100px; margin: 0 auto; padding: 24px; }}
        h1 {{ margin: 0; font-size: 1.6rem; }}
        h2 {{ margin: 0; font-size: 1.05rem; }}
        .subtitle, .timestamp, .muted {{ color: #57606a; }}
        .timestamp {{ margin-top: 4px; font-size: .875rem; }}
        .summary {{ display: flex; flex-wrap: wrap; gap: 8px 20px; margin: 20px 0; padding: 12px 0; border-block: 1px solid #d0d7de; }}
        .summary-item {{ white-space: nowrap; }} .count {{ font-weight: 700; }}
        .add-text {{ color: #1a7f37; }} .change-text {{ color: #9a6700; }} .destroy-text {{ color: #cf222e; }} .read-text {{ color: #0969da; }}
        .resource-section {{ margin: 16px 0; border: 1px solid #d0d7de; border-radius: 6px; }}
        .resource-header {{ display: flex; flex-wrap: wrap; align-items: center; gap: 8px 12px; padding: 12px 16px; background: #f6f8fa; border-bottom: 1px solid #d0d7de; }}
        .source-versions {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(260px, 1fr)); gap: 12px; margin: 20px 0; }}
        .audit-attribution {{ margin: 20px 0; padding: 12px 16px; border: 1px solid #d0d7de; border-radius: 6px; }} .audit-attribution code {{ overflow-wrap: anywhere; }}
        .source-version {{ padding: 12px 16px; border: 1px solid #d0d7de; border-radius: 6px; }}
        .source-version strong, .source-version code, .source-version time {{ display: block; }} .source-version code {{ overflow-wrap: anywhere; }} .source-version time {{ color: #57606a; font-size: .875rem; }}
        .resource-source {{ padding: 6px 16px; color: #57606a; font-size: .875rem; overflow-wrap: anywhere; }} .compact-warning {{ margin: 16px 0; padding: 12px 16px; border-left: 3px solid #bf8700; background: #fff8c5; }} .compact-sources h3 {{ margin: 12px 0 4px; font-size: .95rem; }}
        .resource-name {{ overflow-wrap: anywhere; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; }}
        .action-badge {{ padding: 2px 7px; border: 1px solid currentColor; border-radius: 12px; font-size: .75rem; font-weight: 700; }}
        .action-create {{ color: #1a7f37; }} .action-update {{ color: #9a6700; }} .action-destroy {{ color: #cf222e; }} .action-read {{ color: #0969da; }} .action-moved {{ color: #7d4e00; background: #fff8c5; }} .moved-note {{ color: #7d4e00; }}
        .diff-container {{ overflow-x: auto; }} table {{ width: 100%; border-collapse: collapse; table-layout: fixed; }}
        th, td {{ padding: 8px 12px; text-align: left; vertical-align: top; border-bottom: 1px solid #d8dee4; }} th {{ color: #57606a; background: #fff; font-size: .8rem; font-weight: 600; }} tr:last-child td {{ border-bottom: 0; }}
        td.path {{ width: 22%; color: #57606a; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; overflow-wrap: anywhere; }} td.value {{ overflow-wrap: anywhere; white-space: pre-wrap; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; }}
        .row-add td.after {{ background: #dafbe1; }} .row-remove td.before, .row-change td.before {{ background: #ffebe9; }} .row-change td.after {{ background: #dafbe1; }} .empty {{ color: #8c959f; }}
        .single-column {{ margin: 0; padding: 0; list-style: none; }} .single-column li {{ display: grid; grid-template-columns: minmax(140px, 22%) 1fr; gap: 12px; padding: 8px 12px; border-bottom: 1px solid #d8dee4; }} .single-column li:last-child {{ border-bottom: 0; }}
        .attribute {{ color: #57606a; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; overflow-wrap: anywhere; }} .attribute-value {{ overflow-wrap: anywhere; white-space: pre-wrap; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; }}
        .warnings {{ margin-top: 24px; padding: 12px 16px; border-left: 3px solid #bf8700; background: #fff8c5; }} .warnings h2 {{ margin-bottom: 8px; }} .warning {{ margin: 6px 0; }}
        .batch-summary {{ margin: 16px 0; padding: 12px; border: 1px solid #d0d7de; border-radius: 6px; background: #f6f8fa; }} .batch-event {{ margin: 12px 0; border: 1px solid #d0d7de; border-radius: 6px; overflow: hidden; }} .batch-event summary {{ cursor: pointer; padding: 12px; background: #f6f8fa; font-weight: 600; }} .batch-meta {{ padding: 12px 16px; color: #57606a; font-size: .9rem; }} .batch-diff {{ padding: 0 16px 16px; }} .batch-inventory {{ width: 100%; border-collapse: collapse; }} .batch-inventory td, .batch-inventory th {{ text-align: left; padding: 8px; border-bottom: 1px solid #d0d7de; overflow-wrap: anywhere; }} .batch-error {{ color: #cf222e; }} .batch-event .source-versions {{ margin: 8px 0; }} .batch-event > header {{ display: none; }}
        @media (max-width: 768px) {{ main {{ padding: 16px; }} .diff-container {{ overflow: visible; }} table, tbody, tr, td {{ display: block; width: 100%; }} thead {{ position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; }} tr {{ display: grid; grid-template-columns: 1fr 1fr; border-bottom: 1px solid #d8dee4; }} tr:last-child {{ border-bottom: 0; }} td.path {{ grid-column: 1 / -1; width: auto; padding-bottom: 2px; border-bottom: 0; }} td.value {{ min-width: 0; }} td.before::before, td.after::before {{ display: block; margin-bottom: 4px; color: #57606a; font: 600 .75rem system-ui, sans-serif; content: attr(data-label); }} .single-column li {{ grid-template-columns: 1fr; gap: 2px; }} .batch-inventory th:nth-child(2), .batch-inventory td:nth-child(2) {{ display: none; }} }}
    </style>
</head>
<body>
<main>"#,
        html_escape(title)
    )
}

fn push_summary_item(html: &mut String, count: usize, label: &str, class: &str) {
    if count > 0 {
        html.push_str(&format!(
            "<span class=\"summary-item {}\"><span class=\"count\">{count}</span> {}</span>",
            class,
            html_escape(label)
        ));
    }
}

fn action_label(action: &ActionType) -> (&'static str, &'static str) {
    match action {
        ActionType::Create => ("action-create", "CREATE"),
        ActionType::Update => ("action-update", "UPDATE"),
        ActionType::Destroy => ("action-destroy", "DESTROY"),
        ActionType::Read => ("action-read", "READ"),
        ActionType::NoOp => ("action-read", "NO-OP"),
    }
}

#[derive(Clone, Copy)]
enum ChangeSide {
    Before,
    After,
}

pub(crate) fn render_changes(changes: &[crate::Change], mode: &PlanMode) -> String {
    let (before_label, after_label) = column_labels(mode);
    let mut html = format!(
        "<div class=\"diff-container\"><table><thead><tr><th scope=\"col\">Attribute</th><th scope=\"col\">{}</th><th scope=\"col\">{}</th></tr></thead><tbody>",
        html_escape(before_label),
        html_escape(after_label)
    );

    for change in changes {
        let path = change.path.join(".");
        let row_class = if change.sensitive {
            "row-change"
        } else {
            match (change.before.is_some(), change.after.is_some()) {
                (true, true) => "row-change",
                (true, false) => "row-remove",
                (false, true) => "row-add",
                (false, false) => "row-unchanged",
            }
        };
        let (before, after) = if change.sensitive {
            let marker = if change.before.is_some() && change.after.is_some() {
                "(sensitive value changed)"
            } else {
                "(sensitive value)"
            };
            (
                change.before.as_ref().map(|_| marker.to_string()),
                change.after.as_ref().map(|_| marker.to_string()),
            )
        } else if change.computed {
            (
                change
                    .before
                    .as_ref()
                    .map(|value| format_value(value, true)),
                Some("(known after apply)".to_string()),
            )
        } else {
            (
                change
                    .before
                    .as_ref()
                    .map(|value| format_value(value, true)),
                change.after.as_ref().map(|value| format_value(value, true)),
            )
        };
        html.push_str(&format!(
            "<tr class=\"{row_class}\"><td class=\"path\">{}</td>{}{}</tr>",
            html_escape(&path),
            value_cell("before", before_label, before.as_deref()),
            value_cell("after", after_label, after.as_deref())
        ));
    }
    html.push_str("</tbody></table></div>");
    html
}

fn column_labels(mode: &PlanMode) -> (&'static str, &'static str) {
    match mode {
        PlanMode::StateDiff => ("Older Snapshot", "Later Snapshot"),
        PlanMode::PlanDiff => ("Earlier Plan Report", "Later Plan Report"),
        _ => ("Current State", "After Apply"),
    }
}

fn value_cell(class: &str, label: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!(
            "<td class=\"value {class}\" data-label=\"{}\">{value}</td>",
            html_escape(label)
        ),
        None => format!(
            "<td class=\"value {class} empty\" data-label=\"{}\" aria-label=\"No value\">—</td>",
            html_escape(label)
        ),
    }
}

fn render_fallback_diff(resource: &crate::Resource, mode: &PlanMode) -> String {
    let mut attributes: Vec<_> = resource.attributes.iter().collect();
    attributes.sort_by(|left, right| left.0.cmp(right.0));
    let changes: Vec<_> = attributes
        .into_iter()
        .map(|(key, value)| {
            let (before, after) = match value {
                Value::String(text) if text.contains(" → ") => {
                    let (before, after) = text.split_once(" → ").unwrap();
                    (
                        Some(raw_fallback_value(before)),
                        Some(raw_fallback_value(after)),
                    )
                }
                Value::String(text) if text.starts_with("+ ") => {
                    (None, Some(raw_fallback_value(&text[2..])))
                }
                Value::String(text) if text.starts_with("- ") => {
                    (Some(raw_fallback_value(&text[2..])), None)
                }
                Value::String(text) if text.starts_with("~ ") => {
                    (None, Some(raw_fallback_value(&text[2..])))
                }
                value => {
                    let rendered = format_value(value, true);
                    (Some(rendered.clone()), Some(rendered))
                }
            };
            FallbackRow {
                path: key,
                before,
                after,
            }
        })
        .collect();
    render_fallback_rows(&changes, mode)
}

struct FallbackRow<'a> {
    path: &'a str,
    before: Option<String>,
    after: Option<String>,
}

fn raw_fallback_value(value: &str) -> String {
    format!("<span>{}</span>", html_escape(value))
}

fn render_fallback_rows(rows: &[FallbackRow<'_>], mode: &PlanMode) -> String {
    let (before_label, after_label) = column_labels(mode);
    let mut html = format!(
        "<div class=\"diff-container\"><table><thead><tr><th scope=\"col\">Attribute</th><th scope=\"col\">{}</th><th scope=\"col\">{}</th></tr></thead><tbody>",
        html_escape(before_label),
        html_escape(after_label)
    );
    for row in rows {
        let class = match (row.before.is_some(), row.after.is_some()) {
            (true, true) => "row-change",
            (true, false) => "row-remove",
            (false, true) => "row-add",
            (false, false) => "row-unchanged",
        };
        html.push_str(&format!(
            "<tr class=\"{class}\"><td class=\"path\">{}</td>{}{}</tr>",
            html_escape(row.path),
            value_cell("before", before_label, row.before.as_deref()),
            value_cell("after", after_label, row.after.as_deref())
        ));
    }
    html.push_str("</tbody></table></div>");
    html
}

fn render_attributes(
    attributes: &std::collections::HashMap<String, Value>,
    side: ChangeSide,
) -> String {
    let mut attributes: Vec<_> = attributes.iter().collect();
    attributes.sort_by(|left, right| left.0.cmp(right.0));
    let mut html = String::from("<ul class=\"single-column\">");
    if attributes.is_empty() {
        html.push_str(&format!(
            "<li><span class=\"muted\">{}</span><span class=\"empty\">No attributes</span></li>",
            match side {
                ChangeSide::Before => "Before",
                ChangeSide::After => "After",
            }
        ));
    }
    for (key, value) in attributes {
        html.push_str(&format!(
            "<li><span class=\"attribute\">{}</span><span class=\"attribute-value\">{}</span></li>",
            html_escape(key),
            format_value(value, true)
        ));
    }
    html.push_str("</ul>");
    html
}

fn format_value(value: &Value, with_quotes: bool) -> String {
    match value {
        Value::String(value) => {
            let escaped = html_escape(value);
            if with_quotes {
                format!("&quot;{escaped}&quot;")
            } else {
                escaped
            }
        }
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(values) => {
            if values.is_empty() {
                "[]".to_string()
            } else {
                format!(
                    "[{}]",
                    values
                        .iter()
                        .map(|value| format_value(value, true))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        Value::Object(values) => {
            if values.is_empty() {
                "{}".to_string()
            } else {
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_by(|left, right| left.0.cmp(right.0));
                format!(
                    "{{ {} }}",
                    entries
                        .into_iter()
                        .map(|(key, value)| format!(
                            "{} = {}",
                            html_escape(key),
                            format_value(value, true)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }
}

pub(crate) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
