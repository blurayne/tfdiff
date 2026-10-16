//! Pattern matching shared by resource filters and S3 path selectors.
//!
//! Patterns are globs by default (`--glob`) or regular expressions (`--regex`).
//! Both kinds always match the whole value.

use crate::{DataSource, Resource, Result, TfdiffError};
use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PatternMode {
    #[default]
    Glob,
    Regex,
}

#[derive(Clone, Debug)]
enum Matcher {
    Glob(GlobMatcher),
    Regex(Regex),
}

/// A set of patterns; a value matches when any pattern matches it.
#[derive(Clone, Debug)]
pub struct PatternSet {
    matchers: Vec<Matcher>,
}

impl PatternSet {
    pub fn new(patterns: &[String], mode: PatternMode) -> Result<Self> {
        let matchers = patterns
            .iter()
            .map(|pattern| compile(pattern, mode))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { matchers })
    }

    pub fn is_match(&self, value: &str) -> bool {
        self.matchers.iter().any(|matcher| match matcher {
            Matcher::Glob(glob) => glob.is_match(value),
            Matcher::Regex(regex) => regex.is_match(value),
        })
    }
}

fn compile(pattern: &str, mode: PatternMode) -> Result<Matcher> {
    match mode {
        PatternMode::Glob => GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map(|glob| Matcher::Glob(glob.compile_matcher()))
            .map_err(|error| pattern_error(pattern, "glob", &error.to_string())),
        PatternMode::Regex => anchored_regex(pattern)
            .map(Matcher::Regex)
            .map_err(|error| pattern_error(pattern, "regex", &error.to_string())),
    }
}

/// Guesses whether `value` is written as a regular expression or as a glob.
///
/// Returns `Some(PatternMode::Regex)` only when the value contains a construct that is
/// regex-only (`.*`, `.+`, `\d`, `^`, `$`, `(`, `|`, `+` after a group, `{n}` / `{n,m}`)
/// and no construct that is glob-only (`**`, `{a,b}` alternation). Anything else returns
/// `None`, meaning the caller keeps the glob default. Explicit `--glob` / `--regex` flags
/// always take precedence over this guess.
pub fn detect_pattern_mode(value: &str) -> Option<PatternMode> {
    // A value made only of `.*` is read as a glob, since `*` is the common wildcard there.
    if value.is_empty() || value.replace(".*", "").is_empty() {
        return None;
    }
    if has_glob_only_syntax(value) {
        return None;
    }
    has_regex_only_syntax(value).then_some(PatternMode::Regex)
}

fn has_glob_only_syntax(value: &str) -> bool {
    if value.contains("**") {
        return true;
    }
    // `{a,b}` is glob alternation; `{n}` / `{n,m}` are regex quantifiers.
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        let inner = &after[..close];
        if inner.contains(',') && !is_regex_quantifier(inner) {
            return true;
        }
        rest = &after[close + 1..];
    }
    false
}

fn has_regex_only_syntax(value: &str) -> bool {
    if value.contains(".*") || value.contains(".+") || value.contains('^') || value.contains('$') {
        return true;
    }
    if value.contains("(?") || value.contains('|') || value.contains('\\') {
        return true;
    }
    // A group or class followed by `+` (`(ab)+`, `[a-z]+`) is a regex repetition.
    if value.contains(")+") || value.contains("]+") {
        return true;
    }
    // Quantifier braces: `{3}` or `{1,4}`.
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        if is_regex_quantifier(&after[..close]) {
            return true;
        }
        rest = &after[close + 1..];
    }
    false
}

fn is_regex_quantifier(inner: &str) -> bool {
    let mut parts = inner.splitn(2, ',');
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    match (parts.next(), parts.next()) {
        (Some(min), None) => digits(min),
        (Some(min), Some(max)) => digits(min) && (max.is_empty() || digits(max)),
        _ => false,
    }
}

/// Compiles `pattern` so it has to match the complete value.
pub fn anchored_regex(pattern: &str) -> std::result::Result<Regex, regex::Error> {
    Regex::new(&format!("^(?:{pattern})$"))
}

fn pattern_error(pattern: &str, kind: &str, message: &str) -> TfdiffError {
    TfdiffError::ParseError(format!(
        "Invalid command line arguments: invalid {kind} pattern '{pattern}': {message}"
    ))
}

/// Resource filter built from `--filter-resource-name`, `--filter-resource-arn`
/// and `--filter-resource-id`. Patterns of one flag are OR-ed; flags are AND-ed.
#[derive(Clone, Debug, Default)]
pub struct ResourceFilter {
    name: Option<PatternSet>,
    arn: Option<PatternSet>,
    id: Option<PatternSet>,
}

impl ResourceFilter {
    pub fn new(
        names: &[String],
        arns: &[String],
        ids: &[String],
        mode: PatternMode,
    ) -> Result<Self> {
        let set = |patterns: &[String]| -> Result<Option<PatternSet>> {
            if patterns.is_empty() {
                Ok(None)
            } else {
                PatternSet::new(patterns, mode).map(Some)
            }
        };
        Ok(Self {
            name: set(names)?,
            arn: set(arns)?,
            id: set(ids)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.arn.is_none() && self.id.is_none()
    }

    /// The name filter matches the address, the resource type, or the resource
    /// name. The ARN and ID filters match the `arn`/`id` attribute or any
    /// before/after value of a top-level `arn`/`id` change.
    pub fn matches_resource(&self, resource: &Resource) -> bool {
        self.name.as_ref().is_none_or(|set| {
            [&resource.id, &resource.type_name, &resource.name]
                .iter()
                .any(|value| set.is_match(value))
        }) && self
            .arn
            .as_ref()
            .is_none_or(|set| resource_values(resource, "arn").any(|value| set.is_match(&value)))
            && self
                .id
                .as_ref()
                .is_none_or(|set| resource_values(resource, "id").any(|value| set.is_match(&value)))
    }

    pub fn matches_data_source(&self, data: &DataSource) -> bool {
        let attribute = |key: &str| data.attributes.get(key).and_then(value_text);
        self.name.as_ref().is_none_or(|set| {
            [&data.id, &data.type_name, &data.name]
                .iter()
                .any(|value| set.is_match(value))
        }) && self
            .arn
            .as_ref()
            .is_none_or(|set| attribute("arn").is_some_and(|value| set.is_match(&value)))
            && self
                .id
                .as_ref()
                .is_none_or(|set| attribute("id").is_some_and(|value| set.is_match(&value)))
    }
}

fn resource_values<'a>(resource: &'a Resource, key: &'a str) -> impl Iterator<Item = String> + 'a {
    let from_attributes = resource.attributes.get(key).and_then(value_text);
    let from_changes = resource
        .changes
        .iter()
        .filter(move |change| !change.sensitive && change.path.as_slice() == [key])
        .flat_map(|change| [change.before.as_ref(), change.after.as_ref()])
        .flatten()
        .filter_map(value_text);
    from_attributes.into_iter().chain(from_changes)
}

fn value_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) if text.is_empty() => None,
        serde_json::Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_regex_and_glob_syntax() {
        assert_eq!(
            detect_pattern_mode(r"env/(dev|prod)/.*\.tfstate"),
            Some(PatternMode::Regex)
        );
        assert_eq!(
            detect_pattern_mode("aws_(s3|iam)_.*"),
            Some(PatternMode::Regex)
        );
        assert_eq!(
            detect_pattern_mode("^logs-[a-z]+$"),
            Some(PatternMode::Regex)
        );
        assert_eq!(
            detect_pattern_mode("[a-z]+/state"),
            Some(PatternMode::Regex)
        );
        assert_eq!(detect_pattern_mode("x{3}"), Some(PatternMode::Regex));
        assert_eq!(detect_pattern_mode("x{1,4}"), Some(PatternMode::Regex));
    }

    #[test]
    fn keeps_glob_for_plain_and_glob_values() {
        assert_eq!(detect_pattern_mode("aws_*"), None);
        assert_eq!(detect_pattern_mode("env/*/terraform.tfstate"), None);
        assert_eq!(detect_pattern_mode("env/**/*.tfstate"), None);
        assert_eq!(detect_pattern_mode("env/{dev,prod}/*.tfstate"), None);
        assert_eq!(detect_pattern_mode("aws_security_group.instance[0]"), None);
        assert_eq!(detect_pattern_mode("plain-text"), None);
        assert_eq!(detect_pattern_mode(".*"), None);
        assert_eq!(detect_pattern_mode(".*.*"), None);
        // Glob alternation wins over the regex-looking characters in the same value.
        assert_eq!(detect_pattern_mode("{a,b}*.tfstate"), None);
    }

    use crate::{ActionType, Change};
    use std::collections::HashMap;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn resource(address: &str, attributes: &[(&str, &str)], changes: Vec<Change>) -> Resource {
        let (type_name, name) = address.rsplit_once('.').unwrap();
        Resource {
            id: address.to_string(),
            plan_delta: None,
            name: name.to_string(),
            type_name: type_name.rsplit('.').next().unwrap().to_string(),
            provider: "aws".to_string(),
            action: ActionType::Update,
            changes,
            attributes: attributes
                .iter()
                .map(|(key, value)| (key.to_string(), serde_json::json!(value)))
                .collect::<HashMap<_, _>>(),
            audit_attribution: None,
            applied: false,
            source: None,
        }
    }

    fn change(key: &str, before: &str, after: &str) -> Change {
        Change {
            path: vec![key.to_string()],
            before: Some(serde_json::json!(before)),
            after: Some(serde_json::json!(after)),
            sensitive: false,
            computed: false,
        }
    }

    #[test]
    fn glob_matches_whole_value_and_crosses_separators() {
        let set = PatternSet::new(&strings(&["aws_*"]), PatternMode::Glob).unwrap();
        assert!(set.is_match("aws_s3_bucket.logs"));
        assert!(set.is_match("aws_iam_role"));
        assert!(!set.is_match("module.x.aws_iam_role.y"));
        let arn =
            PatternSet::new(&strings(&["arn:aws:iam::*:role/*app-*"]), PatternMode::Glob).unwrap();
        assert!(arn.is_match("arn:aws:iam::123:role/team/app-web"));
    }

    #[test]
    fn regex_is_anchored_and_invalid_patterns_fail() {
        let set = PatternSet::new(&strings(&["aws_(s3|iam)_.*"]), PatternMode::Regex).unwrap();
        assert!(set.is_match("aws_s3_bucket.logs"));
        assert!(!set.is_match("x.aws_s3_bucket.logs"));
        let error = PatternSet::new(&strings(&["aws_("]), PatternMode::Regex).unwrap_err();
        assert!(error.to_string().contains("invalid regex pattern 'aws_('"));
        let error = PatternSet::new(&strings(&["a[b"]), PatternMode::Glob).unwrap_err();
        assert!(error.to_string().contains("invalid glob pattern 'a[b'"));
    }

    #[test]
    fn resource_filter_ors_patterns_and_ands_flags() {
        let bucket = resource(
            "aws_s3_bucket.logs",
            &[("arn", "arn:aws:s3:::logs")],
            vec![change("id", "logs-old", "logs-new")],
        );
        let role = resource("module.a.aws_iam_role.app", &[("id", "app")], Vec::new());
        let by_type = ResourceFilter::new(
            &strings(&["aws_iam_role", "nope"]),
            &[],
            &[],
            PatternMode::Glob,
        )
        .unwrap();
        assert!(!by_type.matches_resource(&bucket));
        assert!(by_type.matches_resource(&role));

        let by_id =
            ResourceFilter::new(&[], &[], &strings(&["logs-old"]), PatternMode::Glob).unwrap();
        assert!(by_id.matches_resource(&bucket));
        assert!(!by_id.matches_resource(&role));

        let both = ResourceFilter::new(
            &strings(&["aws_s3_*"]),
            &strings(&["arn:aws:s3:::other"]),
            &[],
            PatternMode::Glob,
        )
        .unwrap();
        assert!(!both.matches_resource(&bucket));
        assert!(ResourceFilter::default().is_empty());
    }

    #[test]
    fn sensitive_changes_are_not_matched() {
        let mut secret = change("id", "hidden", "hidden");
        secret.sensitive = true;
        let item = resource("aws_secret.x", &[], vec![secret]);
        let filter =
            ResourceFilter::new(&[], &[], &strings(&["hidden"]), PatternMode::Glob).unwrap();
        assert!(!filter.matches_resource(&item));
    }
}
