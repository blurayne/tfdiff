use crate::parser::StateDiffExport;
use crate::{Result, TfdiffError};

pub fn format_state_diff_export(export: &StateDiffExport) -> Result<String> {
    serde_json::to_string_pretty(export).map_err(TfdiffError::from)
}
