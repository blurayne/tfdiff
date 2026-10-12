use crate::{ActionType, Change, PlanMode, Resource, TerraformPlan, Warning, WarningLevel};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

const SENSITIVE_PLACEHOLDER: &str = "(sensitive value)";

#[derive(Debug, Clone)]
pub struct StateComparison {
    pub plan: TerraformPlan,
    pub export: StateDiffExport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDiffExport {
    pub format_version: String,
    pub terraform_version: String,
    pub resource_changes: Vec<ExportResourceChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResourceChange {
    pub address: String,
    pub mode: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<Value>,
    pub provider_name: String,
    pub change: ExportChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportChange {
    pub actions: Vec<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub before_sensitive: Value,
    pub after_sensitive: Value,
}

#[derive(Debug, Clone)]
struct StateResource {
    address: String,
    mode: String,
    type_name: String,
    name: String,
    index: Option<Value>,
    provider_name: String,
    attributes: Value,
    sensitive_paths: Vec<Vec<PathSegment>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PathSegment {
    Key(String),
    Index(usize),
}

struct Snapshot {
    terraform_version: String,
    lineage: String,
    resources: BTreeMap<String, StateResource>,
}

/// Compare two raw Terraform v4 state snapshots, with `older` as the before state.
pub fn compare_states(older: &str, later: &str) -> crate::Result<StateComparison> {
    let before = parse_state(older)?;
    let after = parse_state(later)?;
    let mut plan = TerraformPlan {
        mode: PlanMode::StateDiff,
        metadata: crate::Metadata {
            terraform_version: Some(after.terraform_version.clone()),
            ..Default::default()
        },
        ..Default::default()
    };

    if !before.lineage.is_empty() && !after.lineage.is_empty() && before.lineage != after.lineage {
        plan.warnings.push(Warning {
            message: "The snapshots have different nonempty lineages; comparison is based on resource addresses only.".to_string(),
            level: WarningLevel::Warning,
        });
    }
    plan.warnings.push(Warning {
        message: "State diff actions describe observed snapshot differences, not Terraform predictions or apply instructions. Root outputs are not included.".to_string(),
        level: WarningLevel::Info,
    });

    let mut addresses: Vec<_> = before
        .resources
        .keys()
        .chain(after.resources.keys())
        .cloned()
        .collect();
    addresses.sort();
    addresses.dedup();

    let mut resource_changes = Vec::new();
    for address in addresses {
        let old = before.resources.get(&address);
        let new = after.resources.get(&address);
        let (action, raw_before, raw_after, mode_source) = match (old, new) {
            (None, Some(current)) => (
                ActionType::Create,
                None,
                Some(current.attributes.clone()),
                current,
            ),
            (Some(previous), None) => (
                ActionType::Destroy,
                Some(previous.attributes.clone()),
                None,
                previous,
            ),
            (Some(previous), Some(current)) => {
                if previous.attributes == current.attributes {
                    continue;
                }
                (
                    ActionType::Update,
                    Some(previous.attributes.clone()),
                    Some(current.attributes.clone()),
                    current,
                )
            }
            (None, None) => continue,
        };
        let mut sensitive_paths = old
            .into_iter()
            .flat_map(|resource| resource.sensitive_paths.clone())
            .chain(
                new.into_iter()
                    .flat_map(|resource| resource.sensitive_paths.clone()),
            )
            .collect::<Vec<_>>();
        sensitive_paths.sort_by_key(|path| format!("{path:?}"));
        sensitive_paths.dedup();

        let before_sensitive = raw_before
            .as_ref()
            .map(|value| redact(value, &sensitive_paths, &[]))
            .transpose()?;
        let after_sensitive = raw_after
            .as_ref()
            .map(|value| redact(value, &sensitive_paths, &[]))
            .transpose()?;
        let mut changes = Vec::new();
        match (&raw_before, &raw_after) {
            (Some(left), Some(right)) => {
                diff_values(left, right, &[], &sensitive_paths, &mut changes)?;
            }
            (None, Some(value)) => {
                collect_leaf_changes(value, &[], true, &sensitive_paths, &mut changes)?;
            }
            (Some(value), None) => {
                collect_leaf_changes(value, &[], false, &sensitive_paths, &mut changes)?;
            }
            (None, None) => {}
        }
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        if changes.is_empty() && action == ActionType::Update {
            continue;
        }
        let chosen_attrs = after_sensitive
            .as_ref()
            .or(before_sensitive.as_ref())
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let attributes = match chosen_attrs {
            Value::Object(values) => values.into_iter().collect(),
            value => HashMap::from([(String::new(), value)]),
        };
        match action {
            ActionType::Create => plan.summary.add += 1,
            ActionType::Update => plan.summary.change += 1,
            ActionType::Destroy => plan.summary.destroy += 1,
            _ => {}
        }
        plan.resources.push(Resource {
            id: address.clone(),
            name: mode_source.name.clone(),
            type_name: mode_source.type_name.clone(),
            provider: mode_source.provider_name.clone(),
            action: action.clone(),
            changes,
            attributes,
            applied: false,
        });
        resource_changes.push(make_export_change(
            mode_source,
            action,
            before_sensitive,
            after_sensitive,
            &sensitive_paths,
        ));
    }

    Ok(StateComparison {
        plan,
        export: StateDiffExport {
            format_version: "1.2".to_string(),
            terraform_version: after.terraform_version,
            resource_changes,
        },
    })
}

fn parse_state(input: &str) -> crate::Result<Snapshot> {
    let root: Value = serde_json::from_str(input)
        .map_err(|_| crate::TfdiffError::ParseError("Invalid Terraform state JSON".to_string()))?;
    let root = root
        .as_object()
        .ok_or_else(|| state_error("top level must be an object"))?;
    let version = root.get("version").and_then(Value::as_u64);
    if version != Some(4) {
        return Err(state_error("only Terraform state version 4 is supported"));
    }
    let terraform_version = required_string(root, "terraform_version")?;
    let lineage = root
        .get("lineage")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let resource_values = root
        .get("resources")
        .and_then(Value::as_array)
        .ok_or_else(|| state_error("resources must be an array"))?;
    let mut resources = BTreeMap::new();

    for resource_value in resource_values {
        let object = resource_value
            .as_object()
            .ok_or_else(|| state_error("each resource must be an object"))?;
        let mode = required_string(object, "mode")?;
        if mode != "managed" && mode != "data" {
            return Err(state_error("unsupported resource mode"));
        }
        let type_name = required_string(object, "type")?;
        let name = required_string(object, "name")?;
        let provider = required_string(object, "provider")?;
        let provider_name = extract_provider_source(&provider)?;
        let module = object
            .get("module")
            .and_then(Value::as_str)
            .map(str::to_string);
        if object.get("module").is_some() && module.is_none() {
            return Err(state_error("module must be a string"));
        }
        let instances = object
            .get("instances")
            .and_then(Value::as_array)
            .ok_or_else(|| state_error("instances must be an array"))?;
        for instance_value in instances {
            let instance = instance_value
                .as_object()
                .ok_or_else(|| state_error("each instance must be an object"))?;
            if instance
                .get("deposed")
                .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty()))
            {
                return Err(state_error("deposed instances are not supported"));
            }
            let index = instance.get("index_key").cloned();
            validate_index(index.as_ref())?;
            let attributes = instance
                .get("attributes")
                .and_then(Value::as_object)
                .ok_or_else(|| state_error("instance attributes must be an object"))?;
            let attributes = Value::Object(attributes.clone());
            let sensitive_paths = parse_sensitive_paths(instance.get("sensitive_attributes"))?;
            validate_sensitive_paths(&attributes, &sensitive_paths)?;
            if sensitive_paths.is_empty() {
                let address =
                    format_address(module.as_deref(), &mode, &type_name, &name, index.as_ref());
                let state_resource = StateResource {
                    address: address.clone(),
                    mode: mode.to_string(),
                    type_name: type_name.clone(),
                    name: name.clone(),
                    index: index.clone(),
                    provider_name: provider_name.clone(),
                    attributes,
                    sensitive_paths,
                };
                if resources.insert(address, state_resource).is_some() {
                    return Err(state_error("duplicate resource instance address"));
                }
                continue;
            }
            let address =
                format_address(module.as_deref(), &mode, &type_name, &name, index.as_ref());
            let state_resource = StateResource {
                address: address.clone(),
                mode: mode.to_string(),
                type_name: type_name.clone(),
                name: name.clone(),
                index,
                provider_name: provider_name.clone(),
                attributes,
                sensitive_paths,
            };
            if resources.insert(address, state_resource).is_some() {
                return Err(state_error("duplicate resource instance address"));
            }
        }
    }

    Ok(Snapshot {
        terraform_version,
        lineage,
        resources,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> crate::Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| state_error(&format!("{key} must be a nonempty string")))
}

fn validate_index(index: Option<&Value>) -> crate::Result<()> {
    if let Some(index) = index {
        match index {
            Value::String(_) => {}
            Value::Number(number) if number.as_u64().is_some() => {}
            _ => {
                return Err(state_error(
                    "instance index_key must be a string or nonnegative integer",
                ))
            }
        }
    }
    Ok(())
}

fn format_address(
    module: Option<&str>,
    mode: &str,
    type_name: &str,
    name: &str,
    index: Option<&Value>,
) -> String {
    let mut address = String::new();
    if let Some(module) = module {
        address.push_str(module);
        address.push('.');
    }
    if mode == "data" {
        address.push_str("data.");
    }
    address.push_str(type_name);
    address.push('.');
    address.push_str(name);
    if let Some(index) = index {
        match index {
            Value::String(value) => {
                address.push('[');
                address.push_str(&serde_json::to_string(value).unwrap_or_default());
                address.push(']');
            }
            Value::Number(value) => address.push_str(&format!("[{value}]")),
            _ => unreachable!("validated index"),
        }
    }
    address
}

fn extract_provider_source(provider: &str) -> crate::Result<String> {
    let start = provider
        .find("provider[\"")
        .ok_or_else(|| state_error("invalid provider configuration address"))?
        + "provider[\"".len();
    let rest = &provider[start..];
    let end = rest
        .find('\"')
        .ok_or_else(|| state_error("invalid provider configuration address"))?;
    let source = &rest[..end];
    if source.is_empty() || source.contains('/') && !source.contains('.') {
        return Err(state_error("invalid provider source address"));
    }
    Ok(source.to_string())
}

fn parse_sensitive_paths(value: Option<&Value>) -> crate::Result<Vec<Vec<PathSegment>>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let paths = value
        .as_array()
        .ok_or_else(|| state_error("sensitive_attributes must be an array"))?;
    paths.iter().map(parse_sensitive_path).collect()
}

fn parse_sensitive_path(path: &Value) -> crate::Result<Vec<PathSegment>> {
    let steps = path
        .as_array()
        .ok_or_else(|| state_error("malformed sensitive attribute path"))?;
    let mut result = Vec::with_capacity(steps.len());
    for step in steps {
        let object = step
            .as_object()
            .ok_or_else(|| state_error("malformed sensitive attribute path step"))?;
        match object.get("type").and_then(Value::as_str) {
            Some("get_attr") => {
                let key = object
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| state_error("malformed sensitive attribute name"))?;
                result.push(PathSegment::Key(key.to_string()));
            }
            Some("index") => {
                let raw = object
                    .get("value")
                    .ok_or_else(|| state_error("malformed sensitive index"))?;
                match raw {
                    Value::Number(number) => {
                        let index = number
                            .as_u64()
                            .and_then(|value| usize::try_from(value).ok())
                            .ok_or_else(|| state_error("unsupported sensitive index encoding"))?;
                        result.push(PathSegment::Index(index));
                    }
                    Value::String(key) => result.push(PathSegment::Key(key.clone())),
                    Value::Object(encoded) => {
                        let value_type = encoded.get("type").and_then(Value::as_str);
                        let value = encoded
                            .get("value")
                            .ok_or_else(|| state_error("malformed sensitive index"))?;
                        match value_type {
                            Some("string") => {
                                let key = value.as_str().ok_or_else(|| {
                                    state_error("malformed sensitive string index")
                                })?;
                                result.push(PathSegment::Key(key.to_string()));
                            }
                            Some("number") | Some("number_integer") => {
                                let number = value.as_u64().or_else(|| {
                                    value.as_str().and_then(|number| number.parse().ok())
                                });
                                let index = number
                                    .and_then(|number| usize::try_from(number).ok())
                                    .ok_or_else(|| {
                                        state_error("malformed sensitive numeric index")
                                    })?;
                                result.push(PathSegment::Index(index));
                            }
                            _ => return Err(state_error("unsupported sensitive index encoding")),
                        }
                    }
                    _ => return Err(state_error("unsupported sensitive index encoding")),
                }
            }
            _ => return Err(state_error("unsupported sensitive attribute path step")),
        }
    }
    Ok(result)
}

fn validate_sensitive_paths(value: &Value, paths: &[Vec<PathSegment>]) -> crate::Result<()> {
    for path in paths {
        let mut current = value;
        for segment in path {
            current = match (segment, current) {
                (PathSegment::Key(key), Value::Object(object)) => {
                    object.get(key).ok_or_else(|| {
                        state_error(
                            "sensitive attribute path does not exist in instance attributes",
                        )
                    })?
                }
                (PathSegment::Index(index), Value::Array(array)) => {
                    array.get(*index).ok_or_else(|| {
                        state_error("sensitive attribute path index is out of bounds")
                    })?
                }
                _ => {
                    return Err(state_error(
                        "sensitive attribute path does not match instance attribute types",
                    ))
                }
            };
        }
    }
    Ok(())
}

fn path_matches(path: &[PathSegment], prefix: &[PathSegment]) -> bool {
    prefix.len() <= path.len() && path[..prefix.len()] == *prefix
}

fn is_sensitive_path(path: &[PathSegment], sensitive_paths: &[Vec<PathSegment>]) -> bool {
    sensitive_paths
        .iter()
        .any(|sensitive| path_matches(path, sensitive))
}

fn has_sensitive_descendant(path: &[PathSegment], sensitive_paths: &[Vec<PathSegment>]) -> bool {
    sensitive_paths
        .iter()
        .any(|sensitive| sensitive.len() > path.len() && path_matches(sensitive, path))
}

fn value_at_path<'a>(value: &'a Value, path: &[PathSegment]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, segment| match (segment, current) {
            (PathSegment::Key(key), Value::Object(object)) => object.get(key),
            (PathSegment::Index(index), Value::Array(array)) => array.get(*index),
            _ => None,
        })
}

fn collect_sensitive_changes(
    before: &Value,
    after: &Value,
    path: &[PathSegment],
    sensitive_paths: &[Vec<PathSegment>],
    changes: &mut Vec<Change>,
) {
    for sensitive_path in sensitive_paths.iter().filter(|sensitive_path| {
        sensitive_path.len() > path.len() && path_matches(sensitive_path, path)
    }) {
        let relative_path = &sensitive_path[path.len()..];
        let before = value_at_path(before, relative_path);
        let after = value_at_path(after, relative_path);
        if before == after {
            continue;
        }
        changes.push(Change {
            path: display_path(sensitive_path),
            before: before.map(|_| Value::String(SENSITIVE_PLACEHOLDER.to_string())),
            after: after.map(|_| Value::String(SENSITIVE_PLACEHOLDER.to_string())),
            sensitive: true,
            computed: false,
        });
    }
}

fn redact(
    value: &Value,
    paths: &[Vec<PathSegment>],
    prefix: &[PathSegment],
) -> crate::Result<Value> {
    if is_sensitive_path(prefix, paths) {
        return Ok(Value::String(SENSITIVE_PLACEHOLDER.to_string()));
    }
    match value {
        Value::Object(object) => {
            let mut redacted = Map::new();
            for (key, child) in object {
                let mut child_path = prefix.to_vec();
                child_path.push(PathSegment::Key(key.clone()));
                redacted.insert(key.clone(), redact(child, paths, &child_path)?);
            }
            Ok(Value::Object(redacted))
        }
        Value::Array(array) => {
            let mut redacted = Vec::with_capacity(array.len());
            for (index, child) in array.iter().enumerate() {
                let mut child_path = prefix.to_vec();
                child_path.push(PathSegment::Index(index));
                redacted.push(redact(child, paths, &child_path)?);
            }
            Ok(Value::Array(redacted))
        }
        _ => Ok(value.clone()),
    }
}

fn diff_values(
    before: &Value,
    after: &Value,
    path: &[PathSegment],
    sensitive_paths: &[Vec<PathSegment>],
    changes: &mut Vec<Change>,
) -> crate::Result<()> {
    if before == after {
        return Ok(());
    }
    if is_sensitive_path(path, sensitive_paths) {
        changes.push(Change {
            path: display_path(path),
            before: Some(Value::String(SENSITIVE_PLACEHOLDER.to_string())),
            after: Some(Value::String(SENSITIVE_PLACEHOLDER.to_string())),
            sensitive: true,
            computed: false,
        });
        return Ok(());
    }
    if has_sensitive_descendant(path, sensitive_paths) {
        collect_sensitive_changes(before, after, path, sensitive_paths, changes);
    }
    if let (Value::Array(left), Value::Array(right)) = (before, after) {
        let mut changed = false;
        for (index, (left, right)) in left.iter().zip(right).enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(PathSegment::Index(index));
            let previous_len = changes.len();
            diff_values(left, right, &child_path, sensitive_paths, changes)?;
            changed |= changes.len() > previous_len;
        }
        changed |= left.len() != right.len();
        if changed || has_sensitive_descendant(path, sensitive_paths) {
            changes.push(Change {
                path: display_path(path),
                before: Some(redact(before, sensitive_paths, path)?),
                after: Some(redact(after, sensitive_paths, path)?),
                sensitive: false,
                computed: false,
            });
        }
        return Ok(());
    }
    if let (Value::Object(left), Value::Object(right)) = (before, after) {
        let initial_changes = changes.len();
        let keys: BTreeMap<_, _> = left
            .keys()
            .chain(right.keys())
            .map(|key| (key.clone(), ()))
            .collect();
        for key in keys.keys() {
            let mut child_path = path.to_vec();
            child_path.push(PathSegment::Key(key.clone()));
            match (left.get(key), right.get(key)) {
                (Some(left), Some(right)) => {
                    diff_values(left, right, &child_path, sensitive_paths, changes)?
                }
                (Some(left), None) => {
                    let masked = redact(left, sensitive_paths, &child_path)?;
                    changes.push(Change {
                        path: display_path(&child_path),
                        before: Some(masked),
                        after: None,
                        sensitive: sensitive_paths.iter().any(|p| path_matches(p, &child_path)),
                        computed: false,
                    });
                }
                (None, Some(right)) => {
                    let masked = redact(right, sensitive_paths, &child_path)?;
                    changes.push(Change {
                        path: display_path(&child_path),
                        before: None,
                        after: Some(masked),
                        sensitive: sensitive_paths.iter().any(|p| path_matches(p, &child_path)),
                        computed: false,
                    });
                }
                (None, None) => {}
            }
        }
        if has_sensitive_descendant(path, sensitive_paths) && changes.len() > initial_changes {
            changes.push(Change {
                path: display_path(path),
                before: Some(redact(before, sensitive_paths, path)?),
                after: Some(redact(after, sensitive_paths, path)?),
                sensitive: false,
                computed: false,
            });
        }
        return Ok(());
    }
    if has_sensitive_descendant(path, sensitive_paths)
        && !matches!((before, after), (Value::Object(_), Value::Object(_)))
        && !matches!((before, after), (Value::Array(_), Value::Array(_)))
    {
        changes.push(Change {
            path: display_path(path),
            before: Some(redact(before, sensitive_paths, path)?),
            after: Some(redact(after, sensitive_paths, path)?),
            sensitive: false,
            computed: false,
        });
        return Ok(());
    }
    let before = redact(before, sensitive_paths, path)?;
    let after = redact(after, sensitive_paths, path)?;
    let sensitive = is_sensitive_path(path, sensitive_paths);
    changes.push(Change {
        path: display_path(path),
        before: Some(before),
        after: Some(after),
        sensitive,
        computed: false,
    });
    Ok(())
}

fn collect_leaf_changes(
    value: &Value,
    path: &[PathSegment],
    add: bool,
    sensitive_paths: &[Vec<PathSegment>],
    changes: &mut Vec<Change>,
) -> crate::Result<()> {
    if is_sensitive_path(path, sensitive_paths) {
        let placeholder = Some(Value::String(SENSITIVE_PLACEHOLDER.to_string()));
        changes.push(Change {
            path: display_path(path),
            before: if add { None } else { placeholder.clone() },
            after: if add { placeholder } else { None },
            sensitive: true,
            computed: false,
        });
        return Ok(());
    }
    if let Value::Object(object) = value {
        if object.is_empty() {
            let value = redact(value, sensitive_paths, path)?;
            changes.push(Change {
                path: display_path(path),
                before: if add { None } else { Some(value.clone()) },
                after: if add { Some(value) } else { None },
                sensitive: false,
                computed: false,
            });
            return Ok(());
        }
        for (key, child) in object {
            let mut child_path = path.to_vec();
            child_path.push(PathSegment::Key(key.clone()));
            collect_leaf_changes(child, &child_path, add, sensitive_paths, changes)?;
        }
        return Ok(());
    }
    let value = redact(value, sensitive_paths, path)?;
    changes.push(Change {
        path: display_path(path),
        before: if add { None } else { Some(value.clone()) },
        after: if add { Some(value) } else { None },
        sensitive: false,
        computed: false,
    });
    Ok(())
}

fn display_path(path: &[PathSegment]) -> Vec<String> {
    path.iter()
        .map(|segment| match segment {
            PathSegment::Key(key) => key.clone(),
            PathSegment::Index(index) => format!("[{index}]"),
        })
        .collect()
}

fn make_export_change(
    resource: &StateResource,
    action: ActionType,
    before: Option<Value>,
    after: Option<Value>,
    sensitive_paths: &[Vec<PathSegment>],
) -> ExportResourceChange {
    let actions = match action {
        ActionType::Create => vec!["create"],
        ActionType::Update => vec!["update"],
        ActionType::Destroy => vec!["delete"],
        _ => vec!["no-op"],
    };
    let before_value = before.unwrap_or(Value::Null);
    let after_value = after.unwrap_or(Value::Null);
    ExportResourceChange {
        address: resource.address.clone(),
        mode: resource.mode.clone(),
        type_name: resource.type_name.clone(),
        name: resource.name.clone(),
        index: resource.index.clone(),
        provider_name: resource.provider_name.clone(),
        change: ExportChange {
            actions: actions.into_iter().map(str::to_string).collect(),
            before_sensitive: sensitivity_mask(&before_value, sensitive_paths, &[]),
            after_sensitive: sensitivity_mask(&after_value, sensitive_paths, &[]),
            before: Some(before_value),
            after: Some(after_value),
        },
    }
}

fn sensitivity_mask(value: &Value, paths: &[Vec<PathSegment>], prefix: &[PathSegment]) -> Value {
    if paths.iter().any(|path| path == prefix) {
        return Value::Bool(true);
    }
    match value {
        Value::Object(object) => {
            let mask = object
                .iter()
                .map(|(key, child)| {
                    let mut child_path = prefix.to_vec();
                    child_path.push(PathSegment::Key(key.clone()));
                    (key.clone(), sensitivity_mask(child, paths, &child_path))
                })
                .collect();
            Value::Object(mask)
        }
        Value::Array(array) => Value::Array(
            array
                .iter()
                .enumerate()
                .map(|(index, child)| {
                    let mut child_path = prefix.to_vec();
                    child_path.push(PathSegment::Index(index));
                    sensitivity_mask(child, paths, &child_path)
                })
                .collect(),
        ),
        _ => Value::Bool(false),
    }
}

fn state_error(message: &str) -> crate::TfdiffError {
    crate::TfdiffError::ParseError(format!("Invalid Terraform v4 state: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(resources: Value) -> String {
        json!({
            "version": 4,
            "terraform_version": "1.8.0",
            "serial": 1,
            "lineage": "lineage",
            "outputs": {},
            "resources": resources
        })
        .to_string()
    }

    fn resource(name: &str, mode: &str, instances: Value) -> Value {
        json!({
            "mode": mode,
            "type": "test_thing",
            "name": name,
            "provider": "provider[\"registry.terraform.io/hashicorp/test\"]",
            "instances": instances
        })
    }

    #[test]
    fn compares_instance_create_update_delete_and_identity() {
        let old = state(json!([
            resource(
                "changed",
                "managed",
                json!([{"attributes":{"nested":{"gone":1,"stay":null},"items":[1,2],"id":"a"}}])
            ),
            resource("removed", "managed", json!([{"attributes":{"x":1}}]))
        ]));
        let new = state(json!([
            resource(
                "changed",
                "managed",
                json!([{"attributes":{"nested":{"stay":null,"added":2},"items":[2,1],"id":"b"}}])
            ),
            resource("added", "managed", json!([{"attributes":{"x":3}}]))
        ]));
        let comparison = compare_states(&old, &new).unwrap();
        assert_eq!(comparison.plan.summary.add, 1);
        assert_eq!(comparison.plan.summary.change, 1);
        assert_eq!(comparison.plan.summary.destroy, 1);
        let changed = comparison
            .plan
            .resources
            .iter()
            .find(|r| r.id == "test_thing.changed")
            .unwrap();
        assert!(changed.changes.iter().any(|c| c.path == ["nested", "gone"]));
        assert!(changed
            .changes
            .iter()
            .any(|c| c.path == ["nested", "added"]));
        assert!(changed.changes.iter().any(|c| c.path == ["items"]));
        assert!(changed.changes.iter().any(|c| c.path == ["id"]));
    }

    #[test]
    fn ignores_bookkeeping_and_warns_on_lineage_change() {
        let old = state(json!([resource(
            "one",
            "managed",
            json!([{"schema_version":1,"attributes":{"x":1},"private":"secret"}])
        )]));
        let mut later: Value = serde_json::from_str(&old).unwrap();
        later["serial"] = json!(99);
        later["lineage"] = json!("another");
        later["resources"][0]["instances"][0]["schema_version"] = json!(2);
        let comparison = compare_states(&old, &later.to_string()).unwrap();
        assert!(comparison.plan.resources.is_empty());
        assert!(comparison
            .plan
            .warnings
            .iter()
            .any(|warning| warning.message.contains("different nonempty lineages")));
    }

    #[test]
    fn encodes_module_data_and_for_each_addresses() {
        let input = state(json!([
            {"module":"module.app[0]","mode":"data","type":"thing","name":"item","provider":"provider[\"registry.terraform.io/acme/test\"].alias","instances":[{"index_key":"a\"b","attributes":{}}]},
            resource("counted", "managed", json!([{"index_key":0,"attributes":{}}]))
        ]));
        let comparison = compare_states(&state(json!([])), &input).unwrap();
        assert!(comparison
            .plan
            .resources
            .iter()
            .any(|r| r.id == "module.app[0].data.thing.item[\"a\\\"b\"]"));
        assert!(comparison
            .plan
            .resources
            .iter()
            .any(|r| r.id == "test_thing.counted[0]"));
    }

    #[test]
    fn redacts_sensitive_change_in_report_and_export() {
        let old = state(json!([resource(
            "secret",
            "managed",
            json!([{
                "attributes":{"object":{"password":"before","visible":"a"}},
                "sensitive_attributes":[[{"type":"get_attr","value":"object"},{"type":"get_attr","value":"password"}]]
            }])
        )]));
        let new = state(json!([resource(
            "secret",
            "managed",
            json!([{
                "attributes":{"object":{"password":"after","visible":"b"}},
                "sensitive_attributes":[[{"type":"get_attr","value":"object"},{"type":"get_attr","value":"password"}]]
            }])
        )]));
        let comparison = compare_states(&old, &new).unwrap();
        let serialized = format!(
            "{}{}",
            serde_json::to_string(&comparison.plan).unwrap(),
            serde_json::to_string(&comparison.export).unwrap()
        );
        assert!(!serialized.contains("\\\"password\\\":\\\"before\\\""));
        assert!(!serialized.contains("\\\"password\\\":\\\"after\\\""));
        assert!(comparison.plan.resources[0]
            .changes
            .iter()
            .any(|change| change.sensitive));
        assert!(serialized.contains(SENSITIVE_PLACEHOLDER));
    }

    #[test]
    fn sensitivity_masks_preserve_nested_object_and_array_shape() {
        let paths = vec![
            vec![
                PathSegment::Key("credentials".to_string()),
                PathSegment::Key("password".to_string()),
            ],
            vec![PathSegment::Key("items".to_string()), PathSegment::Index(1)],
        ];
        let value = json!({
            "credentials":{"password":"secret","visible":"ok"},
            "items":["public","secret"],
            "ordinary":true
        });
        assert_eq!(
            sensitivity_mask(&value, &paths, &[]),
            json!({
                "credentials":{"password":true,"visible":false},
                "items":[false,true],
                "ordinary":false
            })
        );
    }

    #[test]
    fn sensitivity_masks_only_mark_root_for_an_exact_root_path() {
        let value = json!({"nested":{"secret":"hidden","visible":"shown"}});
        let nested_path = vec![vec![
            PathSegment::Key("nested".to_string()),
            PathSegment::Key("secret".to_string()),
        ]];
        assert_eq!(
            sensitivity_mask(&value, &nested_path, &[]),
            json!({"nested":{"secret":true,"visible":false}})
        );
        assert_eq!(sensitivity_mask(&value, &[vec![]], &[]), Value::Bool(true));
    }

    #[test]
    fn parses_string_and_numeric_sensitive_index_encodings() {
        let string_index = json!([{
            "type":"index",
            "value":{"type":"string","value":"named"}
        }]);
        let numeric_index = json!([{
            "type":"index",
            "value":{"type":"number","value":"1"}
        }]);
        assert_eq!(
            parse_sensitive_path(&string_index).unwrap(),
            vec![PathSegment::Key("named".to_string())]
        );
        assert_eq!(
            parse_sensitive_path(&numeric_index).unwrap(),
            vec![PathSegment::Index(1)]
        );
        let value = json!({"named":"secret","other":"public"});
        assert_eq!(
            sensitivity_mask(&value, &[parse_sensitive_path(&string_index).unwrap()], &[]),
            json!({"named":true,"other":false})
        );
        let value = json!(["public", "secret"]);
        assert_eq!(
            sensitivity_mask(
                &value,
                &[parse_sensitive_path(&numeric_index).unwrap()],
                &[]
            ),
            json!([false, true])
        );
    }

    #[test]
    fn rejects_malformed_and_unsupported_sensitive_index_encodings() {
        for path in [
            json!([{"type":"index","value":{"type":"unknown","value":0}}]),
            json!([{"type":"index","value":-1}]),
            json!([{"type":"index","value":{"type":"number","value":"not-a-number"}}]),
            json!([{"type":"get_attr","value":1}]),
        ] {
            assert!(parse_sensitive_path(&path).is_err());
        }
    }

    #[test]
    fn rejects_invalid_state_shapes_and_indices() {
        for input in ["{}", "[]", "{\"format_version\":\"1.0\"}"] {
            assert!(parse_state(input).is_err());
        }
        let invalid = state(json!([resource(
            "bad",
            "managed",
            json!([{"index_key":-1,"attributes":{}}])
        )]));
        assert!(parse_state(&invalid).is_err());
        let unsupported = state(json!([resource("bad", "other", json!([]))]));
        assert!(parse_state(&unsupported).is_err());
    }
}
