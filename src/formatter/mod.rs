pub mod html;
pub mod json;
pub mod markdown;
pub mod terminal;

#[derive(Clone, Copy, Debug)]
pub struct FormatterOptions {
    pub show_resource_identity: bool,
    pub show_full_attribution: bool,
    pub color: bool,
}

impl Default for FormatterOptions {
    fn default() -> Self {
        use std::io::IsTerminal;

        let no_color = std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TFDIFF_NO_COLOR").is_ok_and(|value| {
                matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            });
        let force_color = std::env::var_os("CLICOLOR_FORCE")
            .is_some_and(|value| !value.is_empty() && value != "0");
        let color = !no_color && (std::io::stdout().is_terminal() || force_color);
        Self {
            show_resource_identity: true,
            show_full_attribution: false,
            color,
        }
    }
}

pub use html::*;
pub use json::*;
pub use markdown::*;
pub use terminal::*;

pub(crate) fn resource_identity(resource: &crate::Resource) -> Option<(&'static str, String)> {
    let removed = resource.action == crate::ActionType::Destroy
        || resource.plan_delta == Some(crate::PlanDelta::Removed);
    for key in ["id", "arn", "name"] {
        let from_attributes = resource
            .attributes
            .get(key)
            .filter(|value| !value.is_null() && !value.as_str().is_some_and(str::is_empty))
            .map(|value| (key, identity_value(value)));
        if let Some(identity) = from_attributes {
            return Some(identity);
        }
        if let Some(change) = resource
            .changes
            .iter()
            .find(|change| change.path.as_slice() == [key])
        {
            let value = if change.sensitive {
                None
            } else if removed {
                change.before.as_ref()
            } else {
                change.after.as_ref().or(change.before.as_ref())
            };
            if let Some(value) =
                value.filter(|value| !value.is_null() && !value.as_str().is_some_and(str::is_empty))
            {
                return Some((key, identity_value(value)));
            }
        }
    }
    (!resource.name.is_empty()).then(|| ("name", resource.name.clone()))
}

fn identity_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}
