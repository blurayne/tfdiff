//! Default masking of secret-looking values.
//!
//! On top of the values Terraform itself marks as sensitive, attributes whose key contains
//! `password`, `token`, `secret` or `private_key` (case-insensitive, at any depth) and string
//! values that look like AWS access key IDs (`AKIA…`) are replaced by `(sensitive value)`.
//! `--raw-secrets` turns all masking off, including Terraform's own sensitivity marks.

use crate::{Change, TerraformPlan};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

pub const SENSITIVE_PLACEHOLDER: &str = "(sensitive value)";

const SECRET_KEY_PARTS: [&str; 4] = ["password", "token", "secret", "private_key"];
/// Keys that only reference a secret (`secret_arn`, `token_id`, ...) stay visible.
const REFERENCE_SUFFIXES: [&str; 3] = ["_arn", "_id", "_name"];

static RAW_SECRETS: AtomicBool = AtomicBool::new(false);

/// `true` disables all masking (`--raw-secrets`).
pub fn set_raw_secrets(enabled: bool) {
    RAW_SECRETS.store(enabled, Ordering::Relaxed);
}

pub fn raw_secrets() -> bool {
    RAW_SECRETS.load(Ordering::Relaxed)
}

/// Whether an attribute key names a secret.
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEY_PARTS.iter().any(|part| key.contains(part))
        && !REFERENCE_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
}

/// Whether a value looks like a secret on its own (an AWS access key ID).
pub fn is_secret_value(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        let text = text.trim_matches('"');
        text.len() == 20
            && text.starts_with("AKIA")
            && text
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    })
}

/// Whether a value under a secret key gets masked. Numbers, booleans and null (for example
/// `minimum_password_length` or `token_enabled`) carry no secret and stay visible.
pub fn is_maskable(value: &Value) -> bool {
    match value {
        Value::String(text) => !is_placeholder(text),
        Value::Array(_) | Value::Object(_) => true,
        _ => false,
    }
}

fn is_placeholder(text: &str) -> bool {
    matches!(text, SENSITIVE_PLACEHOLDER | "(known after apply)")
}

fn placeholder() -> Value {
    Value::String(SENSITIVE_PLACEHOLDER.to_string())
}

/// Masks secret-looking values in a parsed text plan, unless `--raw-secrets` is set.
pub fn mask_plan_secrets(plan: &mut TerraformPlan) {
    if raw_secrets() {
        return;
    }
    for resource in &mut plan.resources {
        resource.changes.iter_mut().for_each(mask_change);
        mask_attributes(&mut resource.attributes);
    }
    for data in &mut plan.data_sources {
        mask_attributes(&mut data.attributes);
    }
}

fn mask_change(change: &mut Change) {
    let secret_path = change.path.iter().any(|segment| is_secret_key(segment));
    let mut masked = false;
    for value in [&mut change.before, &mut change.after]
        .into_iter()
        .flatten()
    {
        if secret_path && is_maskable(value) || is_secret_value(value) {
            *value = placeholder();
            masked = true;
        } else {
            masked |= mask_nested(value);
        }
    }
    change.sensitive |= masked;
}

fn mask_attributes(attributes: &mut HashMap<String, Value>) {
    for (key, value) in attributes.iter_mut() {
        if is_secret_key(key) && is_maskable(value) || is_secret_value(value) {
            *value = placeholder();
        } else {
            mask_nested(value);
        }
    }
}

/// Masks secret keys and access key IDs inside objects and arrays; returns whether
/// anything was masked.
fn mask_nested(value: &mut Value) -> bool {
    match value {
        Value::Object(object) => {
            let mut masked = false;
            for (key, child) in object.iter_mut() {
                if is_secret_key(key) && is_maskable(child) || is_secret_value(child) {
                    *child = placeholder();
                    masked = true;
                } else {
                    masked |= mask_nested(child);
                }
            }
            masked
        }
        Value::Array(items) => {
            let mut masked = false;
            for item in items.iter_mut() {
                if is_secret_value(item) {
                    *item = placeholder();
                    masked = true;
                } else {
                    masked |= mask_nested(item);
                }
            }
            masked
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_keys_and_values_are_recognized() {
        for key in [
            "password",
            "master_password",
            "DB_PASSWORD",
            "api_token",
            "client_secret",
            "secret_string",
            "private_key_pem",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in ["name", "secret_arn", "kms_key_id", "token_name", "arn"] {
            assert!(!is_secret_key(key), "{key}");
        }
        assert!(is_secret_value(&json!("AKIAIOSFODNN7EXAMPLE")));
        assert!(!is_secret_value(&json!("AKIA-not-a-key")));
        assert!(!is_maskable(&json!(14)));
        assert!(!is_maskable(&json!("(known after apply)")));
    }

    #[test]
    fn nested_values_are_masked() {
        let mut value = json!({
            "environment": {"variables": {"ACME_API_TOKEN": "t0ken", "REGION": "eu-central-1"}},
            "keys": ["AKIAIOSFODNN7EXAMPLE", "public"],
            "minimum_password_length": 14
        });
        assert!(mask_nested(&mut value));
        assert_eq!(
            value,
            json!({
                "environment": {"variables": {"ACME_API_TOKEN": SENSITIVE_PLACEHOLDER, "REGION": "eu-central-1"}},
                "keys": [SENSITIVE_PLACEHOLDER, "public"],
                "minimum_password_length": 14
            })
        );
    }
}
