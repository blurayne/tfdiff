use crate::{ActionType, TerraformPlan};

fn audit_markdown_code_span(value: &str) -> String {
    let safe_value = value
        .chars()
        .map(|character| match character {
            '\n' | '\r' => ' ',
            character if character.is_control() => '\u{fffd}',
            character => character,
        })
        .take(512)
        .collect::<String>();
    markdown_code_span(&safe_value)
}

pub(crate) fn markdown_code_span(value: &str) -> String {
    let value = value.replace(['\n', '\r'], " ");
    let mut longest_backtick_run = 0;
    let mut current_backtick_run = 0;
    for character in value.chars() {
        if character == '`' {
            current_backtick_run += 1;
            longest_backtick_run = longest_backtick_run.max(current_backtick_run);
        } else {
            current_backtick_run = 0;
        }
    }
    let delimiter = "`".repeat(longest_backtick_run + 1);
    let needs_padding = !value.chars().all(|character| character == ' ')
        && (value.starts_with([' ', '`']) || value.ends_with([' ', '`']));
    let padding = if needs_padding { " " } else { "" };
    format!("{delimiter}{padding}{value}{padding}{delimiter}")
}

pub fn format_markdown_output(plan: &TerraformPlan) -> String {
    format_markdown_output_with_options(plan, super::FormatterOptions::default())
}

pub fn format_markdown_output_with_options(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
) -> String {
    format_markdown_output_with_moves(plan, options, None)
}

/// Like [`format_markdown_output_with_options`], but resources whose address is a key in
/// `moves` get a `🔀 MOVED` marker and the map value as a note.
pub fn format_markdown_output_with_moves(
    plan: &TerraformPlan,
    options: super::FormatterOptions,
    moves: Option<&std::collections::HashMap<String, String>>,
) -> String {
    let mut md = String::new();

    // Title
    md.push_str(&format!(
        "# Terraform {} Report\n\n",
        match plan.mode {
            crate::PlanMode::Plan => "Plan",
            crate::PlanMode::Apply => "Apply",
            crate::PlanMode::StateDiff => "State Snapshot Diff",
            crate::PlanMode::PlanDiff => "Plan Diff",
        }
    ));

    if let Some(attribution) = &plan.author_attribution {
        md.push_str("## CloudTrail Attribution\n\n");
        md.push_str(&format!(
            "- **Audit window:** `{}`–`{}`\n- **Regions:** {}\n- **Status:** {}\n",
            markdown_code_span(&attribution.window_start),
            markdown_code_span(&attribution.window_end),
            if attribution.regions.is_empty() {
                "configured region unavailable".to_string()
            } else {
                attribution
                    .regions
                    .iter()
                    .map(|region| markdown_code_span(region))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            audit_markdown_code_span(&format!("{:?}", attribution.status)),
        ));
        if let Some(message) = &attribution.message {
            md.push_str(&format!("- **Note:** {}\n", markdown_code_span(message)));
        }
        if !attribution.observations.is_empty() {
            md.push_str("\n### Observed resource actors\n\n");
            for observation in &attribution.observations {
                let principal = observation
                    .principal_arn
                    .as_deref()
                    .or(observation.principal_id.as_deref())
                    .unwrap_or("unknown principal");
                md.push_str(&format!(
                    "- {} — {} `{}` at `{}` ({})\n",
                    audit_markdown_code_span(principal),
                    audit_markdown_code_span(&observation.event_name),
                    audit_markdown_code_span(&observation.event_id),
                    audit_markdown_code_span(&observation.event_time),
                    audit_markdown_code_span(&observation.match_basis),
                ));
                if let Some(agent) = &observation.user_agent {
                    md.push_str(&format!(
                        "  - Event userAgent: {}\n",
                        markdown_code_span(agent)
                    ));
                }
            }
        }
        md.push('\n');
    }

    if let Some(versions) = &plan.source_versions {
        md.push_str("## Source Versions\n\n");
        for (label, version) in [
            (&versions.left_label, versions.left.as_ref()),
            (&versions.right_label, versions.right.as_ref()),
        ] {
            if let Some(version) = version {
                md.push_str(&format!(
                    "- **{}:** `{}`{}\n",
                    label,
                    version.version_id.as_deref().unwrap_or("unknown"),
                    version
                        .last_modified
                        .as_deref()
                        .map(|timestamp| format!(" — {timestamp}"))
                        .unwrap_or_default(),
                ));
            }
        }
        md.push('\n');
    }

    // Summary
    md.push_str("## Summary\n\n");
    md.push_str(&format!("- ✅ **{}** resources to add\n", plan.summary.add));
    md.push_str(&format!(
        "- 🔄 **{}** resources to change\n",
        plan.summary.change
    ));
    md.push_str(&format!(
        "- ❌ **{}** resources to destroy\n",
        plan.summary.destroy
    ));
    if plan.summary.read > 0 {
        md.push_str(&format!(
            "- 📖 **{}** resources to read\n",
            plan.summary.read
        ));
    }
    md.push('\n');

    // Resources
    if !plan.resources.is_empty() {
        md.push_str("## Resource Changes\n\n");

        for resource in &plan.resources {
            let (emoji, action_text) = match resource.action {
                ActionType::Create => ("✅", "CREATE"),
                ActionType::Update => ("🔄", "UPDATE"),
                ActionType::Destroy => ("❌", "DESTROY"),
                ActionType::Read => ("📖", "READ"),
                ActionType::NoOp => ("⭕", "NO-OP"),
            };

            if let Some(delta) = &resource.plan_delta {
                let delta_text = match delta {
                    crate::PlanDelta::Added => "ADDED TO REPORT",
                    crate::PlanDelta::Changed => "CHANGED IN REPORT",
                    crate::PlanDelta::Removed => "REMOVED FROM REPORT",
                };
                md.push_str(&format!(
                    "### {} {} `{}`\n\n",
                    emoji, delta_text, resource.id
                ));
            } else {
                let moved = moves.and_then(|moves| moves.get(&resource.id));
                md.push_str(&format!(
                    "### {} {}{} `{}`\n\n",
                    emoji,
                    action_text,
                    if moved.is_some() { " 🔀 MOVED" } else { "" },
                    resource.id
                ));
                if let Some(note) = moved {
                    md.push_str(&format!("- {}\n\n", note));
                }
            }

            if options.show_resource_identity
                && matches!(
                    resource.action,
                    ActionType::Create | ActionType::Update | ActionType::Destroy
                )
            {
                if let Some((key, value)) = super::resource_identity(resource) {
                    md.push_str(&format!("- **{key}:** {}\n\n", markdown_code_span(&value)));
                }
            }

            if let Some(source) = &resource.source {
                md.push_str(&format!("- Source: {}\n\n", markdown_code_span(source)));
            }

            if let Some(attribution) = &resource.audit_attribution {
                if !attribution.observations.is_empty() {
                    md.push_str(&format!(
                        "**Audit attribution:** {} ({} observation(s))\n\n",
                        audit_markdown_code_span(&format!("{:?}", attribution.status)),
                        attribution.observations.len()
                    ));
                    for observation in &attribution.observations {
                        let principal = observation
                            .principal_arn
                            .as_deref()
                            .or(observation.principal_id.as_deref())
                            .unwrap_or("unknown principal");
                        md.push_str(&format!(
                            "- {} — {} {} at {} (match basis: {})\n",
                            audit_markdown_code_span(principal),
                            audit_markdown_code_span(&observation.event_name),
                            audit_markdown_code_span(&observation.event_id),
                            audit_markdown_code_span(&observation.event_time),
                            audit_markdown_code_span(&observation.match_basis),
                        ));
                        if let Some(session_label) = &observation.session_label {
                            md.push_str(&format!(
                                "  - Session: {}\n",
                                audit_markdown_code_span(session_label)
                            ));
                        }
                        if let Some(client_hint) = &observation.client_hint {
                            md.push_str(&format!(
                                "  - Client: {}\n",
                                audit_markdown_code_span(client_hint)
                            ));
                        }
                    }
                    md.push('\n');
                }
            }

            if !resource.changes.is_empty() {
                md.push_str(&markdown_changes_table(&resource.changes));
            } else if !resource.attributes.is_empty() {
                md.push_str("```json\n");
                let mut attributes: Vec<_> = resource.attributes.iter().collect();
                attributes.sort_by(|left, right| left.0.cmp(right.0));
                for (key, value) in attributes {
                    md.push_str(&format!("{}: {}\n", key, value));
                }
                md.push_str("```\n\n");
            }
        }
    }

    // Warnings
    if !plan.warnings.is_empty() {
        md.push_str("## Warnings\n\n");
        for warning in &plan.warnings {
            let level_emoji = match warning.level {
                crate::WarningLevel::Info => "ℹ️",
                crate::WarningLevel::Warning => "⚠️",
                crate::WarningLevel::Error => "🚨",
            };
            md.push_str(&format!("- {} {}\n", level_emoji, warning.message));
        }
        md.push('\n');
    }

    // Metadata
    if let Some(version) = &plan.metadata.terraform_version {
        md.push_str("## Metadata\n\n");
        md.push_str(&format!("- **Terraform Version:** {}\n", version));
        if let Some(timestamp) = &plan.metadata.timestamp {
            md.push_str(&format!("- **Generated:** {}\n", timestamp));
        }
        if let Some(duration) = &plan.metadata.duration {
            md.push_str(&format!("- **Duration:** {}\n", duration));
        }
        md.push('\n');
    }

    md.push_str("---\n");
    md.push_str("*Generated by tfdiff*\n");

    md
}

/// Renders leaf changes as a `| Attribute | Before | After |` table.
pub(crate) fn markdown_changes_table(changes: &[crate::Change]) -> String {
    let mut md = String::new();
    md.push_str("| Attribute | Before | After |\n|---|---|---|\n");
    for change in changes {
        if change.sensitive {
            let marker = if change.before.is_some() && change.after.is_some() {
                "(sensitive value changed; redacted as (sensitive value))"
            } else {
                "(sensitive value)"
            };
            let before = if change.before.is_some() {
                marker
            } else {
                "—"
            };
            let after = if change.after.is_some() {
                marker
            } else {
                "—"
            };
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                change.path.join("."),
                before,
                after
            ));
            continue;
        }
        let before = if change.sensitive {
            if change.before.is_some() && change.after.is_some() {
                "(sensitive value changed)".to_string()
            } else {
                "(sensitive value)".to_string()
            }
        } else {
            change
                .before
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "—".to_string())
        };
        let after = if change.sensitive {
            if change.before.is_some() && change.after.is_some() {
                "(sensitive value changed)".to_string()
            } else {
                "(sensitive value)".to_string()
            }
        } else {
            change
                .after
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "—".to_string())
        };
        md.push_str(&format!(
            "| `{}` | {} | {} |\n",
            change.path.join("."),
            before,
            after
        ));
    }
    md.push('\n');
    md
}
