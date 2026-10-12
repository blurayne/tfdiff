use crate::{ActionType, TerraformPlan};

pub fn format_markdown_output(plan: &TerraformPlan) -> String {
    let mut md = String::new();

    // Title
    md.push_str(&format!(
        "# Terraform {} Report\n\n",
        match plan.mode {
            crate::PlanMode::Plan => "Plan",
            crate::PlanMode::Apply => "Apply",
            crate::PlanMode::StateDiff => "State Snapshot Diff",
        }
    ));

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

            md.push_str(&format!(
                "### {} {} `{}`\n\n",
                emoji, action_text, resource.id
            ));

            if !resource.changes.is_empty() {
                md.push_str("| Attribute | Before | After |\n|---|---|---|\n");
                for change in &resource.changes {
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
