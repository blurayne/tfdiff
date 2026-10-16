use crate::parser::diff::parse_terraform_value;
use crate::{Change, PlanDelta, PlanMode, Resource, Result, Summary, TerraformPlan};
use lazy_static::lazy_static;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub fn parse_terraform_output(input: &str) -> Result<TerraformPlan> {
    let cleaned = crate::parser::clean_input(input)?;
    let lines: Vec<&str> = cleaned.lines().collect();
    let resources = crate::parser::diff::parse_resource_changes(&lines);
    let has_summary = lines
        .iter()
        .any(|line| line.contains("Plan:") || line.contains("Apply complete!"));
    if !has_summary && resources.is_empty() {
        return Err(crate::TfdiffError::ParseError(
            "Input is not a supported Terraform text plan or apply report".to_string(),
        ));
    }

    let mode = detect_mode(&lines)?;
    let summary = extract_summary(&lines)?;
    let warnings = Vec::new(); // TODO: Implement warning extraction

    let mut plan = TerraformPlan {
        mode,
        summary,
        resources,
        warnings,
        ..Default::default()
    };
    crate::secrets::mask_plan_secrets(&mut plan);
    Ok(plan)
}

pub fn compare_plan_outputs(older: &str, later: &str) -> Result<TerraformPlan> {
    let before = parse_terraform_output(older)?;
    let after = parse_terraform_output(later)?;

    let before_resources: BTreeMap<_, _> = before
        .resources
        .into_iter()
        .map(|resource| (resource.id.clone(), resource))
        .collect();
    let after_resources: BTreeMap<_, _> = after
        .resources
        .into_iter()
        .map(|resource| (resource.id.clone(), resource))
        .collect();
    let ids: HashSet<_> = before_resources
        .keys()
        .chain(after_resources.keys())
        .cloned()
        .collect();
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort();

    let mut plan = TerraformPlan {
        mode: PlanMode::PlanDiff,
        ..Default::default()
    };
    for id in ids {
        let old = before_resources.get(&id);
        let new = after_resources.get(&id);
        let (delta, action, changes, attributes, source) = match (old, new) {
            (None, Some(resource)) => (
                PlanDelta::Added,
                resource.action.clone(),
                resource.changes.clone(),
                normalize_plan_attributes(&resource.attributes),
                resource,
            ),
            (Some(resource), None) => (
                PlanDelta::Removed,
                resource.action.clone(),
                resource
                    .changes
                    .iter()
                    .map(|change| Change {
                        path: change.path.clone(),
                        before: change.after.clone().or_else(|| change.before.clone()),
                        after: None,
                        sensitive: change.sensitive,
                        computed: false,
                    })
                    .collect(),
                normalize_plan_attributes(&resource.attributes),
                resource,
            ),
            (Some(previous), Some(current)) => {
                let changes = diff_plan_resource(previous, current);
                if changes.is_empty() && previous.action == current.action {
                    continue;
                }
                (
                    PlanDelta::Changed,
                    current.action.clone(),
                    changes,
                    normalize_plan_attributes(&current.attributes),
                    current,
                )
            }
            (None, None) => continue,
        };
        match delta {
            PlanDelta::Added => plan.summary.add += 1,
            PlanDelta::Changed => plan.summary.change += 1,
            PlanDelta::Removed => plan.summary.destroy += 1,
        }
        plan.resources.push(Resource {
            id: id.clone(),
            plan_delta: Some(delta),
            name: source.name.clone(),
            type_name: source.type_name.clone(),
            provider: source.provider.clone(),
            action,
            changes,
            attributes,
            audit_attribution: None,
            applied: false,
            source: None,
        });
    }
    Ok(plan)
}

fn normalize_plan_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) if value.starts_with("+ ") => parse_terraform_value(value),
        serde_json::Value::String(value) if value.starts_with("- ") => {
            parse_terraform_value(&value[2..])
        }
        value => value.clone(),
    }
}

fn normalize_plan_attributes(
    attributes: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, serde_json::Value> {
    attributes
        .iter()
        .map(|(name, value)| (name.clone(), normalize_plan_value(value)))
        .collect()
}

fn diff_plan_resource(previous: &Resource, current: &Resource) -> Vec<Change> {
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    for change in &previous.changes {
        before.insert(
            change.path.clone(),
            change.after.as_ref().map(normalize_plan_value),
        );
    }
    for change in &current.changes {
        after.insert(
            change.path.clone(),
            change.after.as_ref().map(normalize_plan_value),
        );
    }

    let attribute_names: HashSet<_> = previous
        .attributes
        .keys()
        .chain(current.attributes.keys())
        .cloned()
        .collect();
    for name in attribute_names {
        if !before.contains_key(&vec![name.clone()]) && !after.contains_key(&vec![name.clone()]) {
            before.insert(vec![name.clone()], previous.attributes.get(&name).cloned());
            after.insert(vec![name.clone()], current.attributes.get(&name).cloned());
        }
    }

    let paths: HashSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let old = before.get(&path).cloned().flatten();
            let new = after.get(&path).cloned().flatten();
            if old == new {
                return None;
            }
            let sensitive = previous
                .changes
                .iter()
                .chain(&current.changes)
                .any(|change| change.path == path && change.sensitive);
            let computed = previous
                .changes
                .iter()
                .chain(&current.changes)
                .any(|change| change.path == path && change.computed);
            Some(Change {
                path,
                before: old,
                after: new,
                sensitive,
                computed,
            })
        })
        .collect()
}

pub fn is_terraform_v4_state(input: &str) -> bool {
    serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|value| value.get("version").and_then(Value::as_u64))
        == Some(4)
}

pub fn detect_mode(lines: &[&str]) -> Result<PlanMode> {
    for line in lines {
        if line.contains("Plan:") {
            return Ok(PlanMode::Plan);
        }
        if line.contains("Apply complete!") {
            return Ok(PlanMode::Apply);
        }
    }

    // Default to Plan if unclear
    Ok(PlanMode::Plan)
}

pub fn extract_summary(lines: &[&str]) -> Result<Summary> {
    for line in lines {
        if line.contains("Plan:") {
            return parse_plan_summary(line);
        }
        if line.contains("Apply complete!") {
            return parse_apply_summary(line);
        }
    }

    Ok(Summary::default())
}

lazy_static! {
    static ref PLAN_SUMMARY_REGEX: Regex =
        Regex::new(r"Plan:\s*(\d+)\s*to add,\s*(\d+)\s*to change,\s*(\d+)\s*to destroy").unwrap();
    static ref APPLY_SUMMARY_REGEX: Regex = Regex::new(
        r"Apply complete!\s*Resources:\s*(\d+)\s*added,\s*(\d+)\s*changed,\s*(\d+)\s*destroyed"
    )
    .unwrap();
}

fn parse_plan_summary(line: &str) -> Result<Summary> {
    // Parse "Plan: X to add, Y to change, Z to destroy"

    if let Some(captures) = PLAN_SUMMARY_REGEX.captures(line) {
        let add = captures[1].parse().unwrap_or(0);
        let change = captures[2].parse().unwrap_or(0);
        let destroy = captures[3].parse().unwrap_or(0);

        return Ok(Summary {
            add,
            change,
            destroy,
            read: 0,
        });
    }

    Ok(Summary::default())
}

fn parse_apply_summary(line: &str) -> Result<Summary> {
    // Parse "Apply complete! Resources: X added, Y changed, Z destroyed"

    if let Some(captures) = APPLY_SUMMARY_REGEX.captures(line) {
        let add = captures[1].parse().unwrap_or(0);
        let change = captures[2].parse().unwrap_or(0);
        let destroy = captures[3].parse().unwrap_or(0);

        return Ok(Summary {
            add,
            change,
            destroy,
            read: 0,
        });
    }

    Ok(Summary::default())
}
