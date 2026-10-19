use crate::{ActionType, Change, TerraformPlan};
use colored::*;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;

const INLINE_JSON_LABEL: &str = "(inline JSON string)";

static DUAL_PANE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Selects the update layout: `false` (default) prints `- before` / `+ after` lines,
/// `true` prints the before and after values side by side.
pub fn set_dual_pane(enabled: bool) {
    DUAL_PANE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn dual_pane() -> bool {
    DUAL_PANE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn format_terminal_output(plan: &TerraformPlan) -> String {
    format_terminal_output_with_options(plan, super::FormatterOptions::default())
}

pub fn format_terminal_output_with_options(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
) -> String {
    format_terminal_output_with_s3_provenance(plan, options, None)
}

pub fn format_terminal_output_with_s3_provenance(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
    s3_uri: Option<&str>,
) -> String {
    format_terminal_output_with_s3_provenance_pretty(plan, options, s3_uri, false, false)
}

pub fn format_terminal_output_with_s3_provenance_pretty(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
    s3_uri: Option<&str>,
    pretty: bool,
    show_datetime: bool,
) -> String {
    format_terminal_output_with_s3_provenance_moves(
        plan,
        options,
        s3_uri,
        pretty,
        show_datetime,
        None,
    )
}

/// Like [`format_terminal_output_with_s3_provenance_pretty`], but resources whose address is
/// a key in `moves` get a yellow `MOVED` badge and the map value as a note line.
pub fn format_terminal_output_with_s3_provenance_moves(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
    s3_uri: Option<&str>,
    pretty: bool,
    show_datetime: bool,
    moves: Option<&HashMap<String, String>>,
) -> String {
    colored::control::set_override(options.color);
    let now = chrono::Utc::now();
    let mut output = String::new();

    let is_compact_s3_diff = s3_uri.is_some()
        && matches!(
            plan.mode,
            crate::PlanMode::StateDiff | crate::PlanMode::PlanDiff
        )
        && !pretty;
    if pretty {
        // Decorative header banner, only with --pretty
        output.push_str(&"╔".bright_blue().to_string());
        output.push_str(&"═".repeat(78).bright_blue().to_string());
        output.push_str(&"╗".bright_blue().to_string());
        output.push('\n');

        let mode_text = match plan.mode {
            crate::PlanMode::Plan => "TERRAFORM PLAN ANALYSIS".bright_cyan(),
            crate::PlanMode::Apply => "TERRAFORM APPLY ANALYSIS".bright_yellow(),
            crate::PlanMode::StateDiff => "TERRAFORM STATE SNAPSHOT DIFF".bright_cyan(),
            crate::PlanMode::PlanDiff => "TERRAFORM PLAN DIFF".bright_cyan(),
        };
        output.push_str(&format!("║{:^78}║\n", mode_text));

        output.push_str(&"╚".bright_blue().to_string());
        output.push_str(&"═".repeat(78).bright_blue().to_string());
        output.push_str(&"╝".bright_blue().to_string());
        output.push('\n');
        output.push('\n');
    }

    if let Some(attribution) = plan
        .author_attribution
        .as_ref()
        .filter(|_| options.show_full_attribution)
    {
        if attribution.regions.is_empty() {
            output.push_str(&format!(
                "Audit window: {}–{} | {}\n",
                attribution.window_start,
                attribution.window_end,
                audit_status_label(&attribution.status),
            ));
        } else {
            output.push_str(&format!(
                "Audit window: {}–{} | {} | {}\n",
                attribution.window_start,
                attribution.window_end,
                attribution.regions.join(", "),
                audit_status_label(&attribution.status),
            ));
        }
        if !attribution.observations.is_empty() {
            let principals = attribution
                .observations
                .iter()
                .filter_map(|observation| {
                    observation
                        .principal_arn
                        .as_deref()
                        .or(observation.principal_id.as_deref())
                })
                .collect::<std::collections::HashSet<_>>()
                .len();
            output.push_str(&format!(
                "Observed resource actors: {principals} principal(s) across {} matched resource event(s)\n",
                attribution.observations.len()
            ));
            for observation in &attribution.observations {
                if let Some(principal) = observation
                    .principal_arn
                    .as_deref()
                    .or(observation.principal_id.as_deref())
                {
                    output.push_str(&format!(
                        "  {}\n",
                        sanitize_terminal(principal).bright_white()
                    ));
                }
                if let Some(label) = &observation.session_label {
                    output.push_str(&format!(
                        "  Recorded session label: {}\n",
                        sanitize_terminal(label)
                    ));
                }
                output.push_str(&format!(
                    "  {} | {} | {}\n",
                    sanitize_terminal(&observation.event_name),
                    sanitize_terminal(&observation.event_id),
                    sanitize_terminal(&observation.event_time),
                ));
                if let Some(client) = &observation.client_hint {
                    output.push_str(&format!(
                        "  Client hint: {} (from event userAgent)\n",
                        sanitize_terminal(client)
                    ));
                }
            }
        }
        if let Some(message) = &attribution.message {
            output.push_str(&format!("Audit note: {}\n", sanitize_terminal(message)));
        }
        output.push('\n');
    }

    if let Some(uri) = s3_uri {
        let uri = sanitize_terminal(uri);
        // Rule above each S3 object so consecutive file sections are clearly separated
        output.push_str(&"━".repeat(80).bright_blue().to_string());
        output.push('\n');
        if options.color {
            output.push_str(&format!("\u{1b}[1;97m{uri}\u{1b}[0m\n"));
        } else {
            output.push_str(&format!("{uri}\n"));
        }
        if let Some(versions) = &plan.source_versions {
            for (sign, version) in [
                ('-', versions.left.as_ref()),
                ('+', versions.right.as_ref()),
            ] {
                if let Some(version) = version {
                    output.push_str(&format!(
                        "{} {}  {}\n",
                        sign,
                        sanitize_terminal(version.version_id.as_deref().unwrap_or("unknown")),
                        version
                            .last_modified
                            .as_deref()
                            .map(sanitize_terminal)
                            .map(|timestamp| format!("({timestamp})"))
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        output.push('\n');
    } else if let Some(versions) = &plan.source_versions {
        for (label, version) in [
            (&versions.left_label, versions.left.as_ref()),
            (&versions.right_label, versions.right.as_ref()),
        ] {
            if let Some(version) = version {
                output.push_str(&format!(
                    "{} {}  {}\n",
                    label.bright_white().bold(),
                    version
                        .version_id
                        .as_deref()
                        .unwrap_or("unknown")
                        .bright_cyan(),
                    version
                        .last_modified
                        .as_deref()
                        .map(|timestamp| format!("({timestamp})"))
                        .unwrap_or_default()
                        .dimmed(),
                ));
            }
        }
        output.push('\n');
    }

    // Enhanced Summary Section
    if plan.summary.add > 0
        || plan.summary.change > 0
        || plan.summary.destroy > 0
        || plan.summary.read > 0
    {
        if !is_compact_s3_diff {
            output.push_str("📊 ");
            let summary_title = match plan.mode {
                crate::PlanMode::StateDiff => "OBSERVED SNAPSHOT DIFFERENCES",
                crate::PlanMode::PlanDiff => "PLAN REPORT DIFFERENCES",
                _ => "PLAN SUMMARY",
            };
            output.push_str(&summary_title.bright_white().bold().to_string());
            output.push('\n');
            output.push_str(&"─".repeat(80).dimmed().to_string());
            output.push('\n');
            output.push('\n');
        }

        // Summary stats in a grid layout
        if plan.summary.add > 0 {
            let action_text = match plan.mode {
                crate::PlanMode::StateDiff => "resources observed as added",
                crate::PlanMode::PlanDiff => "resources added to report",
                _ => "resources to add",
            };
            output.push_str(&format!(
                "  {} {} {}\n",
                "➕".bright_green(),
                plan.summary.add.to_string().bright_green().bold(),
                action_text
            ));
        }
        if plan.summary.change > 0 {
            let action_text = match plan.mode {
                crate::PlanMode::StateDiff => "resources observed as changed",
                crate::PlanMode::PlanDiff => "resources changed in report",
                _ => "resources to change",
            };
            output.push_str(&format!(
                "  {} {} {}\n",
                "🔄".bright_yellow(),
                plan.summary.change.to_string().bright_yellow().bold(),
                action_text
            ));
        }
        if plan.summary.destroy > 0 {
            let action_text = match plan.mode {
                crate::PlanMode::StateDiff => "resources observed as removed",
                crate::PlanMode::PlanDiff => "resources removed from report",
                _ => "resources to destroy",
            };
            output.push_str(&format!(
                "  {} {} {}\n",
                "🗑️".bright_red(),
                plan.summary.destroy.to_string().bright_red().bold(),
                action_text
            ));
        }
        if plan.summary.read > 0 {
            output.push_str(&format!(
                "  {} {} data sources to read\n",
                "📖".bright_cyan(),
                plan.summary.read.to_string().bright_cyan().bold()
            ));
        }
        output.push('\n');
        if !is_compact_s3_diff {
            output.push('\n');
        }
    }

    // Resources with enhanced formatting
    if !plan.resources.is_empty() {
        if !is_compact_s3_diff {
            output.push_str("🔧 ");
            output.push_str(&"RESOURCES".bright_white().bold().to_string());
            output.push('\n');
            output.push_str(&"─".repeat(80).dimmed().to_string());
            output.push('\n');
            output.push('\n');
        }

        for resource in &plan.resources {
            let state_date = s3_uri
                .and(plan.source_versions.as_ref())
                .and_then(|versions| match resource.action {
                    ActionType::Destroy => versions.left.as_ref(),
                    _ => versions.right.as_ref(),
                })
                .and_then(|version| version.last_modified.as_deref())
                .map(|timestamp| {
                    if show_datetime {
                        sanitize_terminal(timestamp)
                    } else {
                        format_state_age(timestamp, now)
                    }
                });
            output.push_str(&format_resource_with_options(
                resource,
                options,
                is_compact_s3_diff,
                state_date.as_deref(),
                moves
                    .and_then(|moves| moves.get(&resource.id))
                    .map(String::as_str),
            ));
            output.push('\n');
        }
    }

    // Data sources section
    if !plan.data_sources.is_empty() {
        if !is_compact_s3_diff {
            output.push_str("📊 ");
            output.push_str(&"DATA SOURCES".bright_white().bold().to_string());
            output.push('\n');
            output.push('\n');
        }

        for data_source in &plan.data_sources {
            output.push_str(&format_data_source(data_source, is_compact_s3_diff));
            output.push('\n');
        }
    }

    // Warnings section
    if !plan.warnings.is_empty() {
        if !is_compact_s3_diff {
            output.push_str("⚠️  ");
            output.push_str(&"WARNINGS".bright_yellow().bold().to_string());
            output.push('\n');
            output.push('\n');
        }

        for warning in &plan.warnings {
            output.push_str(&format!(
                "  {} {}\n",
                "•".bright_yellow(),
                warning.message.bright_yellow()
            ));
        }
        output.push('\n');
    }

    output
}

pub(crate) fn format_state_age(timestamp: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let sanitized = sanitize_terminal(timestamp);
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&sanitized) else {
        return sanitized;
    };
    let seconds = now
        .signed_duration_since(parsed.with_timezone(&chrono::Utc))
        .num_seconds();
    let (count, unit) = match seconds {
        s if s < 60 => return "just now".to_string(),
        s if s < 3_600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3_600, "hour"),
        s if s < 30 * 86_400 => (s / 86_400, "day"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "month"),
        s => (s / (365 * 86_400), "year"),
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

fn audit_status_label(status: &crate::authors::AuditStatus) -> &'static str {
    match status {
        crate::authors::AuditStatus::Pending => "lookup pending",
        crate::authors::AuditStatus::Complete => "complete lookup",
        crate::authors::AuditStatus::NoMatch => "no matching event",
        crate::authors::AuditStatus::UnsupportedResource => "unsupported resource",
        crate::authors::AuditStatus::MissingIdentity => "missing safe identity",
        crate::authors::AuditStatus::OutsideRetention => {
            "outside CloudTrail event-history retention"
        }
        crate::authors::AuditStatus::PartialRetention => "partial retention coverage",
        crate::authors::AuditStatus::CrossAccountUnavailable => "cross-account lookup unavailable",
        crate::authors::AuditStatus::Unavailable => "lookup unavailable",
        crate::authors::AuditStatus::Incomplete => "incomplete lookup",
        crate::authors::AuditStatus::Failed => "lookup failed",
    }
}

pub(crate) fn sanitize_terminal(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() && !matches!(*character, '\u{1b}' | '\u{7f}'))
        .take(512)
        .collect()
}

#[allow(dead_code)]
fn audit_status_name(status: &crate::authors::AuditStatus) -> &'static str {
    match status {
        crate::authors::AuditStatus::Pending => "pending",
        crate::authors::AuditStatus::Complete => "complete",
        crate::authors::AuditStatus::NoMatch => "no match",
        crate::authors::AuditStatus::UnsupportedResource => "unsupported resource",
        crate::authors::AuditStatus::MissingIdentity => "missing identity",
        crate::authors::AuditStatus::OutsideRetention => "outside retention",
        crate::authors::AuditStatus::PartialRetention => "partial retention",
        crate::authors::AuditStatus::CrossAccountUnavailable => "cross-account unavailable",
        crate::authors::AuditStatus::Unavailable => "unavailable",
        crate::authors::AuditStatus::Incomplete => "incomplete",
        crate::authors::AuditStatus::Failed => "failed",
    }
}

pub fn format_summary_line(summary: &crate::Summary) -> String {
    let mut parts = Vec::new();

    if summary.add > 0 {
        parts.push(
            format!("✚ {} to add", summary.add)
                .bright_green()
                .to_string(),
        );
    }
    if summary.change > 0 {
        parts.push(
            format!("↻ {} to change", summary.change)
                .bright_yellow()
                .to_string(),
        );
    }
    if summary.destroy > 0 {
        parts.push(
            format!("✖ {} to destroy", summary.destroy)
                .bright_red()
                .to_string(),
        );
    }
    if summary.read > 0 {
        parts.push(
            format!("⇐ {} to read", summary.read)
                .bright_cyan()
                .to_string(),
        );
    }

    parts.join("  │  ")
}

pub fn format_resource(resource: &crate::Resource) -> String {
    format_resource_with_options(
        resource,
        super::FormatterOptions::default(),
        false,
        None,
        None,
    )
}

fn format_resource_with_options(
    resource: &crate::Resource,
    options: super::FormatterOptions,
    _compact: bool,
    state_date: Option<&str>,
    moved: Option<&str>,
) -> String {
    let mut output = String::new();

    // Resource header with action badge
    let action_badge = match resource.action {
        ActionType::Create => format!(" {} ", "CREATE").on_bright_green().black().bold(),
        ActionType::Update => format!(" {} ", "UPDATE").on_bright_yellow().black().bold(),
        ActionType::Destroy => format!(" {} ", "DESTROY").on_bright_red().white().bold(),
        ActionType::Read => format!(" {} ", "READ").on_bright_cyan().black().bold(),
        ActionType::NoOp => format!(" {} ", "NO-OP").on_bright_black().white().bold(),
    };

    let date_suffix = match state_date {
        Some(date)
            if matches!(
                resource.action,
                ActionType::Create | ActionType::Update | ActionType::Destroy
            ) =>
        {
            format!("  {}", date.dimmed())
        }
        _ => String::new(),
    };
    let actor_suffix = resource
        .audit_attribution
        .as_ref()
        .and_then(|attribution| {
            attribution
                .observations
                .iter()
                .find_map(|observation| observation.email_hint.as_deref())
        })
        .map(|email| format!("  @{}", sanitize_terminal(email).bright_white()))
        .unwrap_or_default();
    let moved_badge = if moved.is_some() {
        format!(" {}", " MOVED ".on_yellow().black().bold())
    } else {
        String::new()
    };
    output.push_str(&format!(
        "{}{} {}{}{}\n",
        action_badge,
        moved_badge,
        highlight_address(&resource.id),
        date_suffix,
        actor_suffix
    ));
    if let Some(note) = moved {
        output.push_str(&format!("  {}\n", sanitize_terminal(note).yellow()));
    }
    if options.show_resource_identity
        && matches!(
            resource.action,
            ActionType::Create | ActionType::Update | ActionType::Destroy
        )
    {
        if let Some((key, value)) = super::resource_identity(resource) {
            output.push_str(&format!("  {} {}\n", format!("{key}:").dimmed(), value));
        }
    }

    if let Some(delta) = &resource.plan_delta {
        let delta_text = match delta {
            crate::PlanDelta::Added => "Added to report",
            crate::PlanDelta::Changed => "Changed in report",
            crate::PlanDelta::Removed => "Removed from report",
        };
        output.push_str(&format!("  {}\n", delta_text.dimmed()));
    }

    // Show resource type and provider if available
    if !resource.type_name.is_empty() {
        output.push_str(&format!(
            "  {} {}\n",
            "Type:".dimmed(),
            resource.type_name.bright_blue()
        ));
    }

    if let Some(source) = &resource.source {
        output.push_str(&format!(
            "  {} {}\n",
            "Source:".dimmed(),
            sanitize_terminal(source)
        ));
    }

    // If there are changes, show them in a diff-like format
    if !resource.changes.is_empty() {
        output.push('\n');
        output.push_str(&format_changes(&resource.changes, &resource.action));
    }

    if let Some(attribution) = resource
        .audit_attribution
        .as_ref()
        .filter(|_| options.show_full_attribution)
    {
        if !attribution.observations.is_empty() {
            output.push_str(&format!(
                "  Audit attribution: {} ({} observation(s))\n",
                format!("{:?}", attribution.status).dimmed(),
                attribution.observations.len()
            ));
            for observation in &attribution.observations {
                let principal = observation
                    .principal_arn
                    .as_deref()
                    .or(observation.principal_id.as_deref());
                if let Some(principal) = principal {
                    output.push_str(&format!(
                        "    Principal: {}\n",
                        sanitize_terminal(principal).bright_white()
                    ));
                }
                output.push_str(&format!(
                    "    {} | {} | {}\n",
                    sanitize_terminal(&observation.event_name),
                    sanitize_terminal(&observation.event_id),
                    sanitize_terminal(&observation.event_time),
                ));
                if let Some(session_label) = &observation.session_label {
                    output.push_str(&format!(
                        "    Session: {}\n",
                        sanitize_terminal(session_label)
                    ));
                }
                if let Some(client_hint) = &observation.client_hint {
                    output.push_str(&format!("    Client: {}\n", sanitize_terminal(client_hint)));
                }
                output.push_str(&format!(
                    "    Match basis: {}\n",
                    sanitize_terminal(&observation.match_basis)
                ));
            }
        }
    }

    // Show attributes if no changes are present (for backwards compatibility)
    if resource.changes.is_empty() && !resource.attributes.is_empty() {
        output.push('\n');
        for (key, value) in &resource.attributes {
            // Check if this attribute has arrow notation for side-by-side display
            if let Value::String(val_str) = value {
                if val_str.contains(" → ") {
                    let parts: Vec<&str> = val_str.splitn(2, " → ").collect();
                    if parts.len() == 2 {
                        let before = parts[0].trim();
                        let after = parts[1].trim();

                        // For DESTROY resources with "→ null", just show the removal
                        if resource.action == ActionType::Destroy && after == "null" {
                            output.push_str(&format!(
                                "  {} {} = {}\n",
                                "-".bright_red().bold(),
                                highlight_key(key),
                                before.bright_red()
                            ));
                            continue;
                        }

                        if pretty_inline_json(before).is_some()
                            || pretty_inline_json(after).is_some()
                        {
                            output.push_str(&format!(
                                "  {} {} {}\n",
                                "~".bright_yellow().bold(),
                                highlight_key(key),
                                INLINE_JSON_LABEL.dimmed()
                            ));
                            output.push_str(&format_inline_json_diff(before, after));
                            continue;
                        }

                        output.push_str(&format!(
                            "  {} {}\n",
                            "~".bright_yellow().bold(),
                            highlight_key(key)
                        ));

                        if !dual_pane() {
                            let (before_rendered, after_rendered) =
                                render_value_pair(before, after);
                            output.push_str(&format!(
                                "    {} {}\n",
                                "-".bright_red().bold(),
                                before_rendered
                            ));
                            output.push_str(&format!(
                                "    {} {}\n",
                                "+".bright_green().bold(),
                                after_rendered
                            ));
                            continue;
                        }

                        let max_width = side_by_side_column_width();
                        let before_truncated = if before.len() > max_width {
                            format!("{}...", truncate_utf8(before, max_width - 3))
                        } else {
                            before.to_string()
                        };
                        let after_truncated = if after.len() > max_width {
                            format!("{}...", truncate_utf8(after, max_width - 3))
                        } else {
                            after.to_string()
                        };

                        output.push_str(&format!(
                            "    {} {:<max_width$} │ {} {}\n",
                            "─".bright_red(),
                            before_truncated.bright_red(),
                            "+".bright_green(),
                            after_truncated.bright_green()
                        ));
                        continue;
                    }
                }
            }

            let formatted_value = format_json_value(value, 1);
            output.push_str(&format!("  {} = {}\n", highlight_key(key), formatted_value));
        }
    }

    output
}

pub(crate) fn format_changes(changes: &[Change], _action: &ActionType) -> String {
    let mut output = String::new();

    // Group changes by their path for better organization
    let mut grouped_changes: HashMap<String, Vec<&Change>> = HashMap::new();
    for change in changes {
        let path_key = if change.path.is_empty() {
            "root".to_string()
        } else {
            change.path[0].clone()
        };
        grouped_changes.entry(path_key).or_default().push(change);
    }

    for change in changes {
        let path_str = if change.path.is_empty() {
            "(root)".to_string()
        } else {
            change.path.join(".")
        };

        if change.sensitive {
            let marker = if change.before.is_some() && change.after.is_some() {
                "(sensitive value changed)"
            } else {
                "(sensitive value)"
            };
            let action = match (change.before.is_some(), change.after.is_some()) {
                (true, false) => "-".bright_red().bold(),
                (false, true) => "+".bright_green().bold(),
                _ => "~".bright_yellow().bold(),
            };
            output.push_str(&format!(
                "  {} {} = {}\n",
                action,
                path_str.bright_cyan(),
                marker.italic().dimmed()
            ));
            continue;
        }

        // Handle different change scenarios
        match (&change.before, &change.after) {
            (None, Some(after)) => {
                // Creation
                output.push_str(&format!(
                    "  {} {} = {}\n",
                    "+".bright_green().bold(),
                    path_str.bright_cyan(),
                    format_json_value(after, 2).bright_green()
                ));
            }
            (Some(before), None) => {
                // Deletion
                output.push_str(&format!(
                    "  {} {} = {}\n",
                    "-".bright_red().bold(),
                    path_str.bright_cyan(),
                    format_json_value(before, 2).bright_red()
                ));
            }
            (Some(before), Some(after)) if before != after => {
                // Check if this is a DESTROY action with null after value
                if let ActionType::Destroy = _action {
                    if after == &Value::Null {
                        // For DESTROY with null, just show the value being removed
                        output.push_str(&format!(
                            "  {} {} = {}\n",
                            "-".bright_red().bold(),
                            path_str.bright_cyan(),
                            format_json_value(before, 2).bright_red()
                        ));
                        continue;
                    }
                }

                if let (Some(before_str), Some(after_str)) = (before.as_str(), after.as_str()) {
                    if pretty_inline_json(before_str).is_some()
                        || pretty_inline_json(after_str).is_some()
                    {
                        output.push_str(&format!(
                            "  {} {} {}\n",
                            "~".bright_yellow().bold(),
                            path_str.bright_cyan(),
                            INLINE_JSON_LABEL.dimmed()
                        ));
                        output.push_str(&format_inline_json_diff(before_str, after_str));
                        continue;
                    }
                }

                // Modification - show side by side with aligned columns
                let before_formatted = format_json_value(before, 0);
                let after_formatted = format_json_value(after, 0);

                output.push_str(&format!(
                    "  {} {}\n",
                    "~".bright_yellow().bold(),
                    path_str.bright_cyan()
                ));

                // Calculate column widths for alignment
                if !dual_pane() {
                    let (before_rendered, after_rendered) =
                        render_value_pair(&plain_json_value(before), &plain_json_value(after));
                    output.push_str(&format!(
                        "    {} {}\n",
                        "-".bright_red().bold(),
                        before_rendered
                    ));
                    output.push_str(&format!(
                        "    {} {}\n",
                        "+".bright_green().bold(),
                        after_rendered
                    ));
                    continue;
                }

                let max_width = side_by_side_column_width();
                let before_truncated = if before_formatted.len() > max_width {
                    format!("{}...", truncate_utf8(&before_formatted, max_width - 3))
                } else {
                    before_formatted.clone()
                };
                let after_truncated = if after_formatted.len() > max_width {
                    format!("{}...", truncate_utf8(&after_formatted, max_width - 3))
                } else {
                    after_formatted.clone()
                };

                // Show side-by-side with clear visual separation
                output.push_str(&format!(
                    "    {} {:<max_width$} │ {} {}\n",
                    "─".bright_red(),
                    before_truncated.bright_red(),
                    "+".bright_green(),
                    after_truncated.bright_green()
                ));
            }
            _ => {
                // No change or computed values
                if change.computed {
                    output.push_str(&format!(
                        "  {} {} = {}\n",
                        "?".bright_blue().bold(),
                        path_str.bright_cyan(),
                        "(known after apply)".italic().dimmed()
                    ));
                }
            }
        }

        if change.sensitive {
            output.push_str(&format!(
                "    {} {}\n",
                "🔒",
                "(sensitive value)".italic().dimmed()
            ));
        }
    }

    output
}

/// Renders the before/after pair of a changed value. Unchanged spans keep the plain
/// red/green foreground, while the spans that actually differ are additionally bold and
/// underlined (word-level, similar to `delta`). Returns `(before, after)`.
fn render_value_pair(before: &str, after: &str) -> (String, String) {
    let before_tokens = tokenize_value(before);
    let after_tokens = tokenize_value(after);
    let diff = TextDiff::from_slices(&before_tokens, &after_tokens);
    // Each side is a list of (changed, text) runs; adjacent runs with the same flag merge.
    fn push_run(runs: &mut Vec<(bool, String)>, changed: bool, text: &str) {
        match runs.last_mut() {
            Some((last_changed, last)) if *last_changed == changed => last.push_str(text),
            _ => runs.push((changed, text.to_string())),
        }
    }
    let mut left_runs: Vec<(bool, String)> = Vec::new();
    let mut right_runs: Vec<(bool, String)> = Vec::new();
    for change in diff.iter_all_changes() {
        let text: &str = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                push_run(&mut left_runs, false, text);
                push_run(&mut right_runs, false, text);
            }
            ChangeTag::Delete => push_run(&mut left_runs, true, text),
            ChangeTag::Insert => push_run(&mut right_runs, true, text),
        }
    }
    let style_left = |changed: bool, text: &str| {
        if changed {
            text.bright_red().bold().underline().to_string()
        } else {
            text.bright_red().to_string()
        }
    };
    let style_right = |changed: bool, text: &str| {
        if changed {
            text.bright_green().bold().underline().to_string()
        } else {
            text.bright_green().to_string()
        }
    };
    let left = left_runs
        .iter()
        .map(|(changed, text)| style_left(*changed, text))
        .collect();
    let right = right_runs
        .iter()
        .map(|(changed, text)| style_right(*changed, text))
        .collect();
    (left, right)
}

/// Splits a value into runs of letters and digits and single other characters, so that
/// `task-definition/acme-app:10` vs `:11` only marks `10` and `11`.
fn tokenize_value(value: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut run_start: Option<usize> = None;
    for (index, character) in value.char_indices() {
        if character.is_alphanumeric() {
            run_start.get_or_insert(index);
        } else {
            if let Some(start) = run_start.take() {
                tokens.push(&value[start..index]);
            }
            tokens.push(&value[index..index + character.len_utf8()]);
        }
    }
    if let Some(start) = run_start {
        tokens.push(&value[start..]);
    }
    tokens
}

/// Renders a resource address in bold white.
fn highlight_address(address: &str) -> String {
    style_address_segment(address)
}

/// Styles the plain parts of an address: the `.` separators, numeric indexes such as the
/// `0` in `aws_security_group.instance[0]` and quoted keys such as `"app"` in
/// `aws_ecr_repository.this["app"]` are highlighted, everything else is bold white.
fn style_address_segment(segment: &str) -> String {
    let base = |s: &str| s.bright_white().bold().to_string();
    let highlight = |s: &str| s.bright_yellow().bold().to_string();
    let mut out = String::new();
    let mut rest = segment;
    while let Some(pos) = rest.find(['.', '[']) {
        out.push_str(&base(&rest[..pos]));
        if rest[pos..].starts_with('.') {
            out.push_str(&highlight("."));
            rest = &rest[pos + 1..];
            continue;
        }
        out.push_str(&base("["));
        let after = &rest[pos + 1..];
        match index_end(after) {
            Some(end) => {
                let inner = &after[..end];
                let numeric = !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit());
                let quoted = inner.len() >= 2 && inner.starts_with('"') && inner.ends_with('"');
                if numeric || quoted {
                    out.push_str(&highlight(inner));
                } else {
                    out.push_str(&base(inner));
                }
                out.push_str(&base("]"));
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&base(after));
                rest = "";
            }
        }
    }
    if !rest.is_empty() {
        out.push_str(&base(rest));
    }
    out
}

/// Byte offset of the `]` closing an index, skipping over `]` inside a quoted key.
fn index_end(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (pos, ch) in text.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ']' if !in_quotes => return Some(pos),
            _ => {}
        }
    }
    None
}

/// Renders an attribute key in cyan.
fn highlight_key(key: &str) -> String {
    key.bright_cyan().to_string()
}

/// Width of each column in side-by-side diffs. Fills the terminal width when it is known
/// (via `COLUMNS` or an attached TTY) and never drops below the historical 40 columns.
fn side_by_side_column_width() -> usize {
    const MIN_COLUMN_WIDTH: usize = 40;
    // Indent (4) + "─ " (2) + " │ " (3) + "+ " (2) + trailing margin (1)
    const FIXED_OVERHEAD: usize = 12;
    let total = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .or_else(|| term_size::dimensions().map(|(width, _)| width))
        .unwrap_or(120);
    let available = total.saturating_sub(FIXED_OVERHEAD);
    (available / 2).max(MIN_COLUMN_WIDTH)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn pretty_inline_json(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    serde_json::to_string_pretty(&parsed).ok()
}

fn format_inline_json_diff(before: &str, after: &str) -> String {
    let before_text = pretty_inline_json(before).unwrap_or_else(|| before.to_string());
    let after_text = pretty_inline_json(after).unwrap_or_else(|| after.to_string());
    let diff = TextDiff::from_lines(&before_text, &after_text);

    let mut output = String::new();
    for (group_index, group) in diff.grouped_ops(2).iter().enumerate() {
        if group_index > 0 {
            output.push_str(&format!("      {}\n", "...".dimmed()));
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = change.value().trim_end_matches('\n');
                match change.tag() {
                    ChangeTag::Delete => output.push_str(&format!(
                        "    {} {}\n",
                        "-".bright_red().bold(),
                        line.bright_red()
                    )),
                    ChangeTag::Insert => output.push_str(&format!(
                        "    {} {}\n",
                        "+".bright_green().bold(),
                        line.bright_green()
                    )),
                    ChangeTag::Equal => output.push_str(&format!("      {}\n", line.dimmed())),
                }
            }
        }
    }
    output
}

/// Plain (uncolored) text for a value: strings are shown without their quotes-plus-color
/// wrapper, everything else uses compact JSON. Used as input to `render_value_pair`, which
/// adds the highlighting itself, so no ANSI codes are ever tokenized as text.
fn plain_json_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

fn format_json_value(value: &Value, indent_level: usize) -> String {
    let indent = "  ".repeat(indent_level);

    match value {
        Value::Null => "null".dimmed().to_string(),
        Value::Bool(b) => b.to_string().bright_magenta().to_string(),
        Value::Number(n) => n.to_string().bright_blue().to_string(),
        Value::String(s) => format!("\"{}\"", s).bright_green().to_string(),
        Value::Array(arr) => {
            if arr.is_empty() {
                "[]".to_string()
            } else if arr.len() == 1 {
                format!("[{}]", format_json_value(&arr[0], 0))
            } else {
                let mut result = "[\n".to_string();
                for (i, item) in arr.iter().enumerate() {
                    result.push_str(&format!(
                        "{}  {}",
                        indent,
                        format_json_value(item, indent_level + 1)
                    ));
                    if i < arr.len() - 1 {
                        result.push(',');
                    }
                    result.push('\n');
                }
                result.push_str(&format!("{}]", indent));
                result
            }
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                "{}".to_string()
            } else {
                let mut result = "{\n".to_string();
                let entries: Vec<_> = obj.iter().collect();
                for (i, (key, val)) in entries.iter().enumerate() {
                    result.push_str(&format!(
                        "{}  {} = {}",
                        indent,
                        highlight_key(key),
                        format_json_value(val, indent_level + 1)
                    ));
                    if i < entries.len() - 1 {
                        result.push(',');
                    }
                    result.push('\n');
                }
                result.push_str(&format!("{}}}", indent));
                result
            }
        }
    }
}

fn format_data_source(data_source: &crate::DataSource, _compact: bool) -> String {
    let mut output = String::new();

    let action_badge = format!(" {} ", "READ").on_bright_cyan().black().bold();
    output.push_str(&format!(
        "{} {}\n",
        action_badge,
        data_source.id.bright_white().bold()
    ));

    if !data_source.type_name.is_empty() {
        output.push_str(&format!(
            "  {} {}\n",
            "Type:".dimmed(),
            data_source.type_name.bright_blue()
        ));
    }

    output.push('\n');
    for (key, value) in &data_source.attributes {
        let formatted_value = format_json_value(value, 1);
        output.push_str(&format!("  {} = {}\n", highlight_key(key), formatted_value));
    }

    output
}

#[cfg(test)]
mod state_age_tests {
    use super::format_state_age;
    use chrono::{DateTime, Utc};

    #[test]
    fn state_age_uses_largest_whole_unit() {
        let now = DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_state_age("2026-06-01T11:59:50Z", now), "just now");
        assert_eq!(format_state_age("2026-06-01T11:00:00Z", now), "1 hour ago");
        assert_eq!(format_state_age("2026-06-01T08:00:00Z", now), "4 hours ago");
        assert_eq!(format_state_age("2026-05-31T12:00:00Z", now), "1 day ago");
        assert_eq!(format_state_age("not-a-date", now), "not-a-date");
    }

    #[test]
    fn value_pair_highlights_only_changed_tokens() {
        let _guard = crate::TEST_COLOR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        colored::control::set_override(true);
        let before = "arn:aws:ecs:eu-central-1:111122223333:task-definition/acme-app:10";
        let after = "arn:aws:ecs:eu-central-1:111122223333:task-definition/acme-app:11";
        let (left, right) = super::render_value_pair(before, after);
        // Only the changed number is underlined; the shared prefix is not.
        assert!(left.contains("\u{1b}[1;4;91m10\u{1b}[0m"), "{left:?}");
        assert!(right.contains("\u{1b}[1;4;92m11\u{1b}[0m"), "{right:?}");
        assert!(!left.contains("\u{1b}[1;4;91marn"), "{left:?}");
        assert!(!right.contains("\u{1b}[1;4;92marn"), "{right:?}");
        // No stray escape fragments (ESC split from its sequence) may leak into the output.
        assert!(!left.contains("[0m[9"), "{left:?}");
        assert!(!right.contains("[0m[9"), "{right:?}");
        colored::control::unset_override();
    }

    #[test]
    fn address_highlights_dots_indexes_and_quoted_keys() {
        let _guard = crate::TEST_COLOR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        colored::control::set_override(true);
        use colored::Colorize;
        let yellow = |s: &str| s.bright_yellow().bold().to_string();
        let white = |s: &str| s.bright_white().bold().to_string();

        let quoted = super::style_address_segment(r#"aws_ecr_repository.this["acme-app"]"#);
        assert!(quoted.contains(&yellow(".")), "{quoted:?}");
        assert!(quoted.contains(&yellow(r#""acme-app""#)), "{quoted:?}");
        assert!(quoted.contains(&white("aws_ecr_repository")), "{quoted:?}");

        // Dots and brackets inside the quoted key stay part of the highlighted key.
        let dotted = super::style_address_segment(r#"aws_s3_bucket.b["acme.example]x"]"#);
        assert!(
            dotted.contains(&yellow(r#""acme.example]x""#)),
            "{dotted:?}"
        );

        let numeric = super::style_address_segment("aws_security_group.instance[0]");
        assert!(numeric.contains(&yellow("0")), "{numeric:?}");
        colored::control::unset_override();
    }
}
