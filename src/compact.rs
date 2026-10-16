//! `--compact`: the net change of Terraform state files between two points in time.
//!
//! For every matching S3 key the state as of `--since` (the newest version or delete marker
//! whose `LastModified` is at or before that instant) is compared with the state as of `--until`,
//! skipping every intermediate version.  The per-key results are merged into one
//! [`TerraformPlan`] whose resources carry the key they came from.
//!
//! Only complete objects take part: `ListObjectVersions` returns completed object versions and
//! delete markers only.  Objects of in-progress or aborted multipart uploads are visible solely
//! through `ListMultipartUploads`, which is never called, so they can never become a source.

use crate::formatter::html::{html_document_start, html_escape, render_html_fragment_with_options};
use crate::formatter::markdown::markdown_code_span;
use crate::formatter::terminal::sanitize_terminal;
use crate::formatter::FormatterOptions;
use crate::input::{
    ensure_bucket_versioning_enabled, list_s3_inventory_with_config,
    list_s3_key_versions_with_config, InputSource, S3CachePolicy, S3ClientConfig, S3Inventory,
    S3InventoryObject, S3InventoryScan, S3InventoryVersion, S3VersionReader, VersionSelector,
};
use crate::inventory::{
    classify_state_content, parse_time_range, run_ordered_jobs, InventoryOptions, OrderedJob,
    OrderedJobResult, StateContent,
};
use crate::progress::{ProgressReporter, MAX_CONCURRENT_S3_COMPARISONS};
use crate::{
    compare_states, ActionType, CompactSources, SourceRef, TerraformPlan, TfdiffError, Warning,
};
use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// A syntactically complete, empty Terraform v4 state. Used for the absent side of a comparison.
/// The lineage is empty so no lineage-mismatch warning is produced.
pub const EMPTY_V4_STATE: &str = r#"{"version":4,"terraform_version":"0.0.0","serial":0,"lineage":"","outputs":{},"resources":[]}"#;

#[derive(Clone, Debug, Serialize)]
pub struct CompactReport {
    pub source: String,
    pub complete: bool,
    pub since: Option<String>,
    pub until: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_warning: Option<String>,
    pub counts: CompactCounts,
    pub files: Vec<CompactFile>,
    pub plan: TerraformPlan,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CompactCounts {
    pub matching_keys: usize,
    pub changed_keys: usize,
    pub unchanged_keys: usize,
    pub created_keys: usize,
    pub deleted_keys: usize,
    pub absent_keys: usize,
    pub skipped_non_state: usize,
    pub errors: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompactFile {
    pub key: String,
    pub status: CompactFileStatus,
    pub from: Option<CompactVersion>,
    pub until: Option<CompactVersion>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompactVersion {
    pub version_id: Option<String>,
    pub last_modified: Option<String>,
    pub delete_marker: bool,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactFileStatus {
    Changed,
    Unchanged,
    Created,
    Deleted,
    Absent,
    SkippedNonState,
    Error,
}

#[derive(Clone, Debug)]
pub(crate) struct CompactJob {
    pub(crate) order: usize,
    pub(crate) key: String,
    /// The selected history entry at `--since` (may be a delete marker); `None` when absent.
    pub(crate) from: Option<S3InventoryVersion>,
    /// The selected history entry at `--until` (may be a delete marker); `None` when absent.
    pub(crate) until: Option<S3InventoryVersion>,
}

impl OrderedJob for CompactJob {
    fn order(&self) -> usize {
        self.order
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection<'a> {
    Absent,
    Present(&'a S3InventoryVersion),
    DeleteMarker(&'a S3InventoryVersion),
}

/// Pick the newest history entry whose `LastModified` is at or before `bound`.
///
/// `history` must be sorted newest first (see `compare_inventory_versions`).  A missing bound
/// means "beginning of history" and always selects nothing.
pub(crate) fn select_at_or_before(
    history: &[S3InventoryVersion],
    bound: Option<DateTime<Utc>>,
) -> Result<Selection<'_>, String> {
    let Some(bound) = bound else {
        return Ok(Selection::Absent);
    };
    for entry in history {
        let timestamp = entry
            .last_modified
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .ok_or_else(|| "S3 event has no valid LastModified timestamp".to_string())?;
        if timestamp > bound {
            continue;
        }
        if entry.is_delete_marker {
            return Ok(Selection::DeleteMarker(entry));
        }
        if entry.version_id.is_none() {
            return Err("object version is missing its version ID".to_string());
        }
        return Ok(Selection::Present(entry));
    }
    Ok(Selection::Absent)
}

#[derive(Debug)]
pub(crate) enum KeyPlan {
    /// Decided without reading any object.
    Done(CompactFile),
    /// Needs the contents of the present side(s).
    Job(CompactJob),
}

fn compact_version(entry: &S3InventoryVersion) -> CompactVersion {
    CompactVersion {
        version_id: entry.version_id.clone(),
        last_modified: entry.last_modified.clone(),
        delete_marker: entry.is_delete_marker,
    }
}

fn selection_entry<'a>(selection: Selection<'a>) -> Option<&'a S3InventoryVersion> {
    match selection {
        Selection::Absent => None,
        Selection::Present(entry) | Selection::DeleteMarker(entry) => Some(entry),
    }
}

/// Decide what has to happen for one key.
///
/// | from \ until        | Absent / DeleteMarker                 | Present(u)                          |
/// |---------------------|---------------------------------------|-------------------------------------|
/// | Absent / DeleteMarker | `Absent`, no reads                  | create job: read u                  |
/// | Present(f)          | destroy job: read f                   | same version: `Unchanged`; else read f + u |
pub(crate) fn plan_key(
    order: usize,
    object: &S3InventoryObject,
    since: Option<DateTime<Utc>>,
    until: DateTime<Utc>,
) -> KeyPlan {
    let selected = select_at_or_before(&object.history, since).and_then(|from| {
        select_at_or_before(&object.history, Some(until)).map(|until| (from, until))
    });
    let (from, until) = match selected {
        Ok(pair) => pair,
        Err(reason) => {
            return KeyPlan::Done(CompactFile {
                key: object.key.clone(),
                status: CompactFileStatus::Error,
                from: None,
                until: None,
                reason: Some(reason),
            })
        }
    };
    let file = |status| CompactFile {
        key: object.key.clone(),
        status,
        from: selection_entry(from).map(compact_version),
        until: selection_entry(until).map(compact_version),
        reason: None,
    };
    match (from, until) {
        (
            Selection::Absent | Selection::DeleteMarker(_),
            Selection::Absent | Selection::DeleteMarker(_),
        ) => KeyPlan::Done(file(CompactFileStatus::Absent)),
        (Selection::Present(f), Selection::Present(u)) if f.version_id == u.version_id => {
            KeyPlan::Done(file(CompactFileStatus::Unchanged))
        }
        _ => KeyPlan::Job(CompactJob {
            order,
            key: object.key.clone(),
            from: selection_entry(from).cloned(),
            until: selection_entry(until).cloned(),
        }),
    }
}

/// Result of reading and comparing the present sides of one job.
#[derive(Clone, Debug)]
pub(crate) enum JobOutput {
    Plan(Box<TerraformPlan>),
    NonState(String),
}

/// Compare raw contents of the two sides; an absent side is the empty v4 state.
pub(crate) fn compare_loaded(from: Option<&str>, until: Option<&str>) -> Result<JobOutput, String> {
    for contents in [from, until].into_iter().flatten() {
        match classify_state_content(contents) {
            StateContent::TerraformV4 => {}
            StateContent::Unrelated => {
                return Ok(JobOutput::NonState(
                    "object contents are not Terraform v4 state JSON".to_string(),
                ))
            }
            StateContent::Invalid(message) => return Err(message),
        }
    }
    compare_states(
        from.unwrap_or(EMPTY_V4_STATE),
        until.unwrap_or(EMPTY_V4_STATE),
    )
    .map(|comparison| JobOutput::Plan(Box::new(comparison.plan)))
    .map_err(|error| error.to_string())
}

fn is_present(entry: &Option<S3InventoryVersion>) -> bool {
    entry.as_ref().is_some_and(|entry| !entry.is_delete_marker)
}

async fn run_job(
    reader: S3VersionReader,
    inventory: S3Inventory,
    job: CompactJob,
) -> Result<JobOutput, String> {
    let load = |entry: &Option<S3InventoryVersion>| {
        let reader = reader.clone();
        let inventory = inventory.clone();
        let key = job.key.clone();
        let entry = entry.clone().filter(|entry| !entry.is_delete_marker);
        async move {
            match entry {
                Some(entry) => reader
                    .load_selected(&inventory, &key, &entry)
                    .await
                    .map(|loaded| Some(loaded.contents))
                    .map_err(|error| error.to_string()),
                None => Ok(None),
            }
        }
    };
    let (from, until) = tokio::try_join!(load(&job.from), load(&job.until))?;
    compare_loaded(from.as_deref(), until.as_deref())
}

/// Result for one key: its file record and, when it produced one, its per-key plan.
#[derive(Clone, Debug)]
pub(crate) struct KeyOutcome {
    pub(crate) file: CompactFile,
    pub(crate) plan: Option<TerraformPlan>,
}

/// Turn the output of a finished job into a [`KeyOutcome`].
pub(crate) fn job_outcome(
    job: &CompactJob,
    result: Result<JobOutput, String>,
) -> (KeyOutcome, Option<String>) {
    let base = |status, reason: Option<String>| CompactFile {
        key: job.key.clone(),
        status,
        from: job.from.as_ref().map(compact_version),
        until: job.until.as_ref().map(compact_version),
        reason,
    };
    match result {
        Ok(JobOutput::Plan(plan)) => {
            let status = if !is_present(&job.from) && is_present(&job.until) {
                CompactFileStatus::Created
            } else if is_present(&job.from) && !is_present(&job.until) {
                CompactFileStatus::Deleted
            } else if plan.resources.is_empty() {
                CompactFileStatus::Unchanged
            } else {
                CompactFileStatus::Changed
            };
            (
                KeyOutcome {
                    file: base(status, None),
                    plan: Some(*plan),
                },
                None,
            )
        }
        Ok(JobOutput::NonState(reason)) => (
            KeyOutcome {
                file: base(CompactFileStatus::SkippedNonState, Some(reason)),
                plan: None,
            },
            None,
        ),
        Err(message) => (
            KeyOutcome {
                file: base(CompactFileStatus::Error, Some(message.clone())),
                plan: None,
            },
            Some(format!("{}: {message}", job.key)),
        ),
    }
}

/// Build the net-change report for an S3 prefix, glob, or exact object.
///
/// Checks that bucket versioning is `Enabled` before listing anything.
pub async fn build_compact_report(
    source: &InputSource,
    options: &InventoryOptions,
    cache_policy: &S3CachePolicy,
    s3_config: &S3ClientConfig,
    line_progress: bool,
    color: bool,
) -> crate::Result<CompactReport> {
    let now = Utc::now();
    let (since, until) = parse_time_range(options, now)?;
    let until = until.unwrap_or(now);
    let (bucket, label) = match source {
        InputSource::S3(object) if object.selector == VersionSelector::Current => (
            object.bucket.clone(),
            format!("s3://{}/{}", object.bucket, object.key),
        ),
        InputSource::S3Inventory(inventory) => (
            inventory.bucket.clone(),
            format!("s3://{}/{}", inventory.bucket, inventory.selector_path()),
        ),
        _ => {
            return Err(TfdiffError::InputError {
                input: "--compact".to_string(),
                message: "--compact requires exactly one S3 prefix, glob, or exact object input"
                    .to_string(),
            })
        }
    };

    ensure_bucket_versioning_enabled(s3_config, &bucket).await?;
    let scan = match source {
        InputSource::S3(object) => {
            list_s3_key_versions_with_config(&bucket, &object.key, s3_config).await?
        }
        InputSource::S3Inventory(inventory) => {
            list_s3_inventory_with_config(inventory, s3_config).await?
        }
        InputSource::Local(_) => unreachable!("validated above"),
    };

    let mut outcomes = Vec::new();
    let mut errors = Vec::new();
    let mut jobs = Vec::new();
    for (order, object) in scan.objects.iter().enumerate() {
        match plan_key(order, object, since, until) {
            KeyPlan::Done(file) => {
                if file.status == CompactFileStatus::Error {
                    errors.push(format!(
                        "{}: {}",
                        file.key,
                        file.reason.as_deref().unwrap_or("unknown error")
                    ));
                }
                outcomes.push(KeyOutcome { file, plan: None });
            }
            KeyPlan::Job(job) => jobs.push(job),
        }
    }

    if !jobs.is_empty() {
        let inventory = S3Inventory {
            bucket: bucket.clone(),
            selector: crate::input::InventorySelector::Prefix(String::new()),
        };
        let reader = S3VersionReader::new_with_config(cache_policy, s3_config).await?;
        let total = jobs.len();
        let progress = ProgressReporter::new(
            total,
            "S3 compact",
            &format!("comparing {total} state files"),
            line_progress,
            color,
        );
        let results = run_ordered_jobs(
            jobs,
            MAX_CONCURRENT_S3_COMPARISONS,
            move |job: CompactJob| {
                let reader = reader.clone();
                let inventory = inventory.clone();
                async move { run_job(reader, inventory, job).await }
            },
            |job: &CompactJob| {
                if let Some(progress) = progress.as_ref() {
                    progress.start(&format!("reading {}", job.key));
                }
            },
            |job: &CompactJob, task_failed| {
                if let Some(progress) = progress.as_ref() {
                    let detail = if task_failed {
                        "comparison task failed".to_string()
                    } else {
                        format!("compared {}", job.key)
                    };
                    progress.complete(&detail);
                }
            },
        )
        .await;
        let done = results.len();
        for result in results {
            let value = match result.result {
                OrderedJobResult::Completed(value) => value,
                OrderedJobResult::TaskFailed(message) => {
                    Err(format!("comparison task failed: {message}"))
                }
            };
            let (outcome, error) = job_outcome(&result.job, value);
            errors.extend(error);
            outcomes.push(outcome);
        }
        if let Some(progress) = progress {
            progress.finish(&format!("{done}/{total} comparisons processed"));
        }
    }

    Ok(assemble_report(
        label,
        since,
        until,
        &scan_with_errors(&scan),
        outcomes,
        errors,
    ))
}

fn scan_with_errors(scan: &S3InventoryScan) -> (usize, Vec<String>) {
    (scan.objects.len(), scan.errors.clone())
}

/// Merge per-key outcomes into one report. Pure: no I/O.
pub(crate) fn assemble_report(
    source: String,
    since: Option<DateTime<Utc>>,
    until: DateTime<Utc>,
    scan: &(usize, Vec<String>),
    mut outcomes: Vec<KeyOutcome>,
    key_errors: Vec<String>,
) -> CompactReport {
    let (matching_keys, scan_errors) = scan;
    outcomes.sort_by(|left, right| left.file.key.cmp(&right.file.key));
    let mut errors = scan_errors.clone();
    errors.extend(key_errors);

    let mut counts = CompactCounts {
        matching_keys: *matching_keys,
        errors: errors.len(),
        ..Default::default()
    };
    let mut plan = TerraformPlan {
        mode: crate::PlanMode::StateDiff,
        ..Default::default()
    };
    let mut versions = BTreeSet::new();
    let mut warnings_seen = BTreeSet::new();
    let mut files = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        match outcome.file.status {
            CompactFileStatus::Changed => counts.changed_keys += 1,
            CompactFileStatus::Unchanged => counts.unchanged_keys += 1,
            CompactFileStatus::Created => counts.created_keys += 1,
            CompactFileStatus::Deleted => counts.deleted_keys += 1,
            CompactFileStatus::Absent => counts.absent_keys += 1,
            CompactFileStatus::SkippedNonState => counts.skipped_non_state += 1,
            CompactFileStatus::Error => {}
        }
        if let Some(key_plan) = outcome.plan {
            let until_present = outcome
                .file
                .until
                .as_ref()
                .is_some_and(|version| !version.delete_marker);
            if until_present {
                versions.insert(key_plan.metadata.terraform_version.clone());
            }
            for warning in key_plan.warnings {
                let message = format!("{}: {}", outcome.file.key, warning.message);
                if warnings_seen.insert((message.clone(), format!("{:?}", warning.level))) {
                    plan.warnings.push(Warning {
                        message,
                        level: warning.level,
                    });
                }
            }
            for mut resource in key_plan.resources {
                resource.source = Some(outcome.file.key.clone());
                plan.resources.push(resource);
            }
        }
        files.push(outcome.file);
    }
    if versions.len() == 1 {
        plan.metadata.terraform_version = versions.into_iter().next().flatten();
    }
    plan.source_versions = None;
    plan.resources
        .sort_by(|a, b| (&a.source, &a.id).cmp(&(&b.source, &b.id)));

    let since_text = since.map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
    let since_warning = since_text.as_ref().map(|since| {
        format!(
            "--compact with --since: only changes after {since} are shown; history before that point is collapsed into the starting state, so the result may be incomplete (resources from earlier versions are not listed as changes)"
        )
    });
    let mut report = CompactReport {
        source,
        complete: errors.is_empty(),
        since: since_text,
        until: until.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        since_warning,
        counts,
        files,
        plan,
        errors,
    };
    refresh_compact_sources(&mut report);
    report
}

/// Recompute `plan.compact_sources` and `plan.summary` from the resources currently in the plan.
///
/// Call again after filtering resources so the per-action source lists stay in sync.
pub fn refresh_compact_sources(report: &mut CompactReport) {
    let mut groups: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut summary = crate::Summary::default();
    for resource in &report.plan.resources {
        let group = match resource.action {
            ActionType::Create => {
                summary.add += 1;
                "create"
            }
            ActionType::Update => {
                summary.change += 1;
                "update"
            }
            ActionType::Destroy => {
                summary.destroy += 1;
                "destroy"
            }
            ActionType::Read => {
                summary.read += 1;
                continue;
            }
            ActionType::NoOp => continue,
        };
        if let Some(source) = &resource.source {
            groups.entry(group).or_default().insert(source.clone());
        }
    }
    let refs = |group: &str| -> Vec<SourceRef> {
        groups
            .get(group)
            .into_iter()
            .flatten()
            .map(|key| source_ref_for(report, key))
            .collect()
    };
    let sources = CompactSources {
        create: refs("create"),
        update: refs("update"),
        destroy: refs("destroy"),
    };
    report.plan.compact_sources = Some(sources);
    report.plan.summary = summary;
}

/// Versions of `key` at the from and until side of the report.
pub fn source_ref_for(report: &CompactReport, key: &str) -> SourceRef {
    let file = report.files.iter().find(|file| file.key == key);
    let version = |side: Option<&CompactVersion>| {
        side.filter(|version| !version.delete_marker)
            .and_then(|version| version.version_id.clone())
    };
    SourceRef {
        key: key.to_string(),
        from_version: version(file.and_then(|file| file.from.as_ref())),
        until_version: version(file.and_then(|file| file.until.as_ref())),
        deleted: file.is_some_and(|file| file.status == CompactFileStatus::Deleted),
    }
}

/// Decides which source information is rendered. Call after [`refresh_compact_sources`].
///
/// - `quiet` removes the per-action source lists and every per-resource source.
/// - `atomic` expands each resource source to its key and versions
///   (`key  (from -> until) [deleted]`) for human-readable output. JSON keeps the plain
///   key, since `files` already carries the versions of every key.
pub fn apply_source_display(
    report: &mut CompactReport,
    quiet: bool,
    atomic: bool,
    structured: bool,
) {
    if quiet {
        report.plan.compact_sources = None;
        for resource in &mut report.plan.resources {
            resource.source = None;
        }
        return;
    }
    if !atomic || structured {
        return;
    }
    let lines: Vec<Option<String>> = report
        .plan
        .resources
        .iter()
        .map(|resource| {
            resource
                .source
                .as_ref()
                .map(|key| source_line(&source_ref_for(report, key)))
        })
        .collect();
    for (resource, line) in report.plan.resources.iter_mut().zip(lines) {
        resource.source = line;
    }
}

// ---------------------------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------------------------

fn header_line(report: &CompactReport) -> String {
    let counts = &report.counts;
    format!(
        "S3 compact: {} matching keys | {} changed | {} created | {} deleted | {} unchanged | {} non-state skipped | {} errors | since {} | until {}{}",
        counts.matching_keys,
        counts.changed_keys,
        counts.created_keys,
        counts.deleted_keys,
        counts.unchanged_keys,
        counts.skipped_non_state,
        counts.errors,
        report.since.as_deref().unwrap_or("beginning"),
        report.until,
        if report.complete { "" } else { " | INCOMPLETE" }
    )
}

fn source_groups(sources: &CompactSources) -> [(&'static str, &Vec<SourceRef>); 3] {
    [
        ("Create sources", &sources.create),
        ("Update sources", &sources.update),
        ("Destroy sources", &sources.destroy),
    ]
}

pub fn source_line(source: &SourceRef) -> String {
    format!(
        "{}  ({} -> {}){}",
        source.key,
        source.from_version.as_deref().unwrap_or("(none)"),
        source.until_version.as_deref().unwrap_or("(none)"),
        if source.deleted { " [deleted]" } else { "" }
    )
}

/// Plain-text block listing the contributing files per action. Empty groups are omitted.
pub fn format_sources_block(sources: &CompactSources) -> String {
    let mut output = String::new();
    for (title, refs) in source_groups(sources) {
        if refs.is_empty() {
            continue;
        }
        output.push_str(&format!("{title}:\n"));
        for source in refs {
            output.push_str(&format!("  {}\n", sanitize_terminal(&source_line(source))));
        }
    }
    output
}

fn skipped_and_errors(report: &CompactReport, quiet: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if !quiet {
        for file in report
            .files
            .iter()
            .filter(|file| file.status == CompactFileStatus::SkippedNonState)
        {
            lines.push(format!(
                "- {}: {}",
                file.key,
                file.reason.as_deref().unwrap_or("not Terraform v4 state")
            ));
        }
    }
    lines
}

pub fn format_compact_terminal(
    report: &CompactReport,
    options: FormatterOptions,
    quiet: bool,
    pretty: bool,
    show_datetime: bool,
) -> String {
    colored::control::set_override(options.color);
    let mut output = format!("{}\n", header_line(report));
    if let Some(warning) = &report.since_warning {
        output.push_str(&format!("{}\n", format!("Warning: {warning}").yellow()));
    }
    if let Some(sources) = &report.plan.compact_sources {
        let block = format_sources_block(sources);
        if !block.is_empty() {
            output.push('\n');
            output.push_str(&block);
        }
    }
    output.push('\n');
    output.push_str(
        &crate::formatter::format_terminal_output_with_s3_provenance_pretty(
            &report.plan,
            options,
            None,
            pretty,
            show_datetime,
        ),
    );
    let skipped = skipped_and_errors(report, quiet);
    if !skipped.is_empty() {
        output.push_str("\nSkipped (not Terraform v4 state):\n");
        for line in skipped {
            output.push_str(&format!("{}\n", sanitize_terminal(&line)));
        }
    }
    if !report.errors.is_empty() {
        output.push_str("\nErrors:\n");
        for error in &report.errors {
            output.push_str(&format!("- {}\n", sanitize_terminal(error)));
        }
    }
    output
}

pub fn format_compact_markdown_with_options(
    report: &CompactReport,
    options: FormatterOptions,
) -> String {
    let mut output = format!(
        "# S3 Compact Net Change\n\nSource: {}  \nSince: {} · Until: {}  \nMatching keys: {} · Changed: {} · Created: {} · Deleted: {} · Unchanged: {} · Non-state skipped: {} · Errors: {}{}\n",
        markdown_code_span(&report.source),
        markdown_code_span(report.since.as_deref().unwrap_or("beginning")),
        markdown_code_span(&report.until),
        report.counts.matching_keys,
        report.counts.changed_keys,
        report.counts.created_keys,
        report.counts.deleted_keys,
        report.counts.unchanged_keys,
        report.counts.skipped_non_state,
        report.counts.errors,
        if report.complete { "" } else { " · **INCOMPLETE**" }
    );
    if let Some(warning) = &report.since_warning {
        output.push_str(&format!("\n> **Warning:** {warning}\n"));
    }
    if let Some(sources) = &report.plan.compact_sources {
        for (title, refs) in source_groups(sources) {
            if refs.is_empty() {
                continue;
            }
            output.push_str(&format!("\n## {title}\n\n"));
            for source in refs {
                output.push_str(&format!(
                    "- {} ({} -> {}){}\n",
                    markdown_code_span(&source.key),
                    source.from_version.as_deref().unwrap_or("(none)"),
                    source.until_version.as_deref().unwrap_or("(none)"),
                    if source.deleted { " [deleted]" } else { "" }
                ));
            }
        }
    }
    output.push('\n');
    output.push_str(&crate::formatter::format_markdown_output_with_options(
        &report.plan,
        options,
    ));
    let skipped = skipped_and_errors(report, false);
    if !skipped.is_empty() {
        output.push_str("\n## Skipped (not Terraform v4 state)\n\n");
        for file in report
            .files
            .iter()
            .filter(|file| file.status == CompactFileStatus::SkippedNonState)
        {
            output.push_str(&format!(
                "- {}: {}\n",
                markdown_code_span(&file.key),
                file.reason.as_deref().unwrap_or("not Terraform v4 state")
            ));
        }
    }
    if !report.errors.is_empty() {
        output.push_str("\n## Errors\n\n");
        for error in &report.errors {
            output.push_str(&format!("- {error}\n"));
        }
    }
    output
}

pub fn format_compact_markdown(report: &CompactReport) -> String {
    format_compact_markdown_with_options(report, FormatterOptions::default())
}

pub fn format_compact_html_with_options(
    report: &CompactReport,
    options: FormatterOptions,
) -> String {
    let mut html = html_document_start("S3 Terraform compact change");
    html.push_str(&format!(
        "<h1>S3 Terraform compact change</h1><div class=\"batch-summary\">{} matching keys · {} changed · {} created · {} deleted · {} unchanged · {} non-state skipped · {} errors · since {} · until {}{}</div>",
        report.counts.matching_keys,
        report.counts.changed_keys,
        report.counts.created_keys,
        report.counts.deleted_keys,
        report.counts.unchanged_keys,
        report.counts.skipped_non_state,
        report.counts.errors,
        html_escape(report.since.as_deref().unwrap_or("beginning")),
        html_escape(&report.until),
        if report.complete { "" } else { " · incomplete" }
    ));
    if let Some(warning) = &report.since_warning {
        html.push_str(&format!(
            "<div class=\"compact-warning\"><strong>Warning:</strong> {}</div>",
            html_escape(warning)
        ));
    }
    if let Some(sources) = &report.plan.compact_sources {
        html.push_str("<section class=\"compact-sources\">");
        for (title, refs) in source_groups(sources) {
            if refs.is_empty() {
                continue;
            }
            html.push_str(&format!("<h3>{}</h3><ul>", html_escape(title)));
            for source in refs {
                html.push_str(&format!(
                    "<li><code>{}</code> ({} -&gt; {}){}</li>",
                    html_escape(&source.key),
                    html_escape(source.from_version.as_deref().unwrap_or("(none)")),
                    html_escape(source.until_version.as_deref().unwrap_or("(none)")),
                    if source.deleted { " [deleted]" } else { "" }
                ));
            }
            html.push_str("</ul>");
        }
        html.push_str("</section>");
    }
    html.push_str(&render_html_fragment_with_options(&report.plan, options));
    let skipped: Vec<_> = report
        .files
        .iter()
        .filter(|file| file.status == CompactFileStatus::SkippedNonState)
        .collect();
    if !skipped.is_empty() {
        html.push_str("<h2>Skipped (not Terraform v4 state)</h2><ul>");
        for file in skipped {
            html.push_str(&format!(
                "<li><code>{}</code>: {}</li>",
                html_escape(&file.key),
                html_escape(file.reason.as_deref().unwrap_or("not Terraform v4 state"))
            ));
        }
        html.push_str("</ul>");
    }
    if !report.errors.is_empty() {
        html.push_str("<h2 class=\"batch-error\">Errors</h2><ul>");
        for error in &report.errors {
            html.push_str(&format!("<li>{}</li>", html_escape(error)));
        }
        html.push_str("</ul>");
    }
    html.push_str("</main></body></html>");
    html
}

pub fn format_compact_html(report: &CompactReport) -> String {
    format_compact_html_with_options(report, FormatterOptions::default())
}

pub fn format_compact_json(report: &CompactReport) -> crate::Result<String> {
    serde_json::to_string_pretty(report).map_err(TfdiffError::from)
}

/// Short summary: header line, optional warning, and resource counts.
pub fn format_compact_summary(report: &CompactReport) -> String {
    let mut output = format!("{}\n", header_line(report));
    if let Some(warning) = &report.since_warning {
        output.push_str(&format!("Warning: {warning}\n"));
    }
    let summary = &report.plan.summary;
    output.push_str(&format!(
        "{} to add, {} to change, {} to destroy\n",
        summary.add, summary.change, summary.destroy
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Resource, Summary};
    use chrono::TimeZone;

    fn render_terminal(report: &CompactReport, options: FormatterOptions, quiet: bool) -> String {
        let _guard = crate::TEST_COLOR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        format_compact_terminal(report, options, quiet, false, false)
    }

    fn ts(hour: u32) -> String {
        format!("2026-01-01T{hour:02}:00:00Z")
    }

    fn bound(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
    }

    fn version(id: &str, hour: u32, marker: bool, latest: bool) -> S3InventoryVersion {
        S3InventoryVersion {
            version_id: Some(id.to_string()),
            last_modified: Some(ts(hour)),
            is_delete_marker: marker,
            is_latest: latest,
            size: Some(1),
            etag: None,
        }
    }

    fn object(key: &str, history: Vec<S3InventoryVersion>) -> S3InventoryObject {
        S3InventoryObject {
            key: key.to_string(),
            history,
        }
    }

    fn state(resources: &[(&str, &str)]) -> String {
        let items: Vec<String> = resources
            .iter()
            .map(|(name, value)| {
                format!(
                    r#"{{"mode":"managed","type":"aws_s3_bucket","name":"{name}","provider":"provider[\"registry.terraform.io/hashicorp/aws\"]","instances":[{{"attributes":{{"bucket":"{value}"}}}}]}}"#
                )
            })
            .collect();
        format!(
            r#"{{"version":4,"terraform_version":"1.9.0","serial":1,"lineage":"abc","outputs":{{}},"resources":[{}]}}"#,
            items.join(",")
        )
    }

    fn plan_of(from: Option<&str>, until: Option<&str>) -> TerraformPlan {
        match compare_loaded(from, until).unwrap() {
            JobOutput::Plan(plan) => *plan,
            JobOutput::NonState(_) => panic!("expected plan"),
        }
    }

    // ---- select_at_or_before ----

    #[test]
    fn select_without_bound_is_absent() {
        let history = vec![version("v1", 1, false, true)];
        assert_eq!(select_at_or_before(&history, None), Ok(Selection::Absent));
    }

    #[test]
    fn select_picks_version_written_long_before_bound() {
        let history = vec![
            version("v2", 10, false, true),
            version("v1", 1, false, false),
        ];
        let selected = select_at_or_before(&history, Some(bound(5))).unwrap();
        assert_eq!(selected, Selection::Present(&history[1]));
    }

    #[test]
    fn select_boundary_is_inclusive_and_later_is_skipped() {
        let history = vec![
            version("v2", 10, false, true),
            version("v1", 5, false, false),
        ];
        assert_eq!(
            select_at_or_before(&history, Some(bound(5))).unwrap(),
            Selection::Present(&history[1])
        );
        assert_eq!(
            select_at_or_before(&history, Some(bound(4))).unwrap(),
            Selection::Absent
        );
    }

    #[test]
    fn select_tie_prefers_latest_entry_first_in_order() {
        let history = vec![
            version("new", 5, false, true),
            version("old", 5, false, false),
        ];
        assert_eq!(
            select_at_or_before(&history, Some(bound(5))).unwrap(),
            Selection::Present(&history[0])
        );
    }

    #[test]
    fn select_delete_marker() {
        let history = vec![version("m", 8, true, true), version("v1", 1, false, false)];
        assert_eq!(
            select_at_or_before(&history, Some(bound(9))).unwrap(),
            Selection::DeleteMarker(&history[0])
        );
        assert_eq!(
            select_at_or_before(&history, Some(bound(7))).unwrap(),
            Selection::Present(&history[1])
        );
    }

    #[test]
    fn select_missing_timestamp_is_error() {
        let mut entry = version("v1", 1, false, true);
        entry.last_modified = None;
        let error = select_at_or_before(&[entry], Some(bound(5))).unwrap_err();
        assert!(error.contains("LastModified"));
    }

    #[test]
    fn select_null_version_id_is_a_valid_id() {
        let history = vec![version("null", 1, false, true)];
        assert_eq!(
            select_at_or_before(&history, Some(bound(5))).unwrap(),
            Selection::Present(&history[0])
        );
    }

    #[test]
    fn select_missing_version_id_is_error() {
        let mut entry = version("v1", 1, false, true);
        entry.version_id = None;
        assert!(select_at_or_before(&[entry], Some(bound(5))).is_err());
    }

    // ---- plan_key decision table ----

    fn plan(history: Vec<S3InventoryVersion>, since: Option<u32>, until: u32) -> KeyPlan {
        plan_key(0, &object("k", history), since.map(bound), bound(until))
    }

    #[test]
    fn plan_absent_to_absent_has_no_reads() {
        // Created and deleted inside the window.
        let history = vec![version("m", 6, true, true), version("v1", 3, false, false)];
        match plan(history, Some(2), 9) {
            KeyPlan::Done(file) => assert_eq!(file.status, CompactFileStatus::Absent),
            KeyPlan::Job(_) => panic!("expected no job"),
        }
    }

    #[test]
    fn plan_delete_marker_to_delete_marker_is_absent() {
        let history = vec![
            version("m2", 8, true, true),
            version("m1", 3, true, false),
            version("v1", 1, false, false),
        ];
        match plan(history, Some(4), 9) {
            KeyPlan::Done(file) => assert_eq!(file.status, CompactFileStatus::Absent),
            KeyPlan::Job(_) => panic!("expected no job"),
        }
    }

    #[test]
    fn plan_absent_to_present_is_create_job() {
        let history = vec![version("v1", 3, false, true)];
        match plan(history, Some(2), 9) {
            KeyPlan::Job(job) => {
                assert!(job.from.is_none());
                assert_eq!(job.until.unwrap().version_id.as_deref(), Some("v1"));
            }
            KeyPlan::Done(_) => panic!("expected job"),
        }
        match plan(vec![version("v1", 3, false, true)], None, 9) {
            KeyPlan::Job(job) => assert!(job.from.is_none()),
            KeyPlan::Done(_) => panic!("expected job"),
        }
    }

    #[test]
    fn plan_present_to_delete_marker_is_destroy_job() {
        let history = vec![version("m", 6, true, true), version("v1", 1, false, false)];
        match plan(history, Some(2), 9) {
            KeyPlan::Job(job) => {
                assert_eq!(job.from.unwrap().version_id.as_deref(), Some("v1"));
                assert!(job.until.unwrap().is_delete_marker);
            }
            KeyPlan::Done(_) => panic!("expected job"),
        }
    }

    #[test]
    fn plan_same_version_is_unchanged_without_reads() {
        let history = vec![version("v1", 1, false, true)];
        match plan(history, Some(2), 9) {
            KeyPlan::Done(file) => {
                assert_eq!(file.status, CompactFileStatus::Unchanged);
                assert_eq!(file.from, file.until);
            }
            KeyPlan::Job(_) => panic!("expected no job"),
        }
    }

    #[test]
    fn plan_different_versions_is_job_over_both() {
        let history = vec![
            version("v3", 8, false, true),
            version("v2", 5, false, false),
            version("v1", 1, false, false),
        ];
        match plan(history, Some(2), 9) {
            KeyPlan::Job(job) => {
                assert_eq!(job.from.unwrap().version_id.as_deref(), Some("v1"));
                assert_eq!(job.until.unwrap().version_id.as_deref(), Some("v3"));
            }
            KeyPlan::Done(_) => panic!("expected job"),
        }
    }

    #[test]
    fn plan_selection_error_becomes_error_file() {
        let mut entry = version("v1", 1, false, true);
        entry.last_modified = Some("garbage".into());
        match plan(vec![entry], Some(2), 9) {
            KeyPlan::Done(file) => assert_eq!(file.status, CompactFileStatus::Error),
            KeyPlan::Job(_) => panic!("expected error"),
        }
    }

    // ---- empty state / compare_loaded ----

    #[test]
    fn empty_state_compares_without_warnings() {
        let current = state(&[("a", "one")]);
        let created = plan_of(None, Some(&current));
        assert_eq!(created.summary.add, 1);
        assert!(created.warnings.is_empty());
        let destroyed = plan_of(Some(&current), None);
        assert_eq!(destroyed.summary.destroy, 1);
        assert!(destroyed.warnings.is_empty());
    }

    #[test]
    fn compare_loaded_non_state_is_skipped() {
        let result = compare_loaded(None, Some("hello world")).unwrap();
        assert!(matches!(result, JobOutput::NonState(_)));
        let result = compare_loaded(Some(&state(&[])), Some(r#"{"foo":1}"#)).unwrap();
        assert!(matches!(result, JobOutput::NonState(_)));
    }

    #[test]
    fn compare_loaded_invalid_state_is_error() {
        assert!(compare_loaded(None, Some(r#"{"version":3,"resources":[]}"#)).is_err());
    }

    // ---- merge / refresh ----

    fn outcome(
        key: &str,
        status: CompactFileStatus,
        from: Option<&str>,
        until: Option<&str>,
        plan: Option<TerraformPlan>,
    ) -> KeyOutcome {
        let side = |id: &str| CompactVersion {
            version_id: Some(id.to_string()),
            last_modified: Some(ts(1)),
            delete_marker: false,
        };
        KeyOutcome {
            file: CompactFile {
                key: key.to_string(),
                status,
                from: from.map(side),
                until: until.map(side),
                reason: None,
            },
            plan,
        }
    }

    fn report_of(
        since: Option<DateTime<Utc>>,
        outcomes: Vec<KeyOutcome>,
        errors: Vec<String>,
    ) -> CompactReport {
        let total = outcomes.len();
        assemble_report(
            "s3://b/p/**".into(),
            since,
            bound(12),
            &(total, Vec::new()),
            outcomes,
            errors,
        )
    }

    #[test]
    fn merge_keeps_duplicate_addresses_per_file() {
        let a = plan_of(Some(&state(&[("x", "1")])), Some(&state(&[("x", "2")])));
        let b = plan_of(None, Some(&state(&[("x", "9")])));
        let report = report_of(
            None,
            vec![
                outcome(
                    "b/two.tfstate",
                    CompactFileStatus::Created,
                    None,
                    Some("v9"),
                    Some(b),
                ),
                outcome(
                    "a/one.tfstate",
                    CompactFileStatus::Changed,
                    Some("v1"),
                    Some("v2"),
                    Some(a),
                ),
            ],
            vec![],
        );
        assert_eq!(report.plan.resources.len(), 2);
        let sources: Vec<_> = report
            .plan
            .resources
            .iter()
            .map(|r| r.source.clone().unwrap())
            .collect();
        assert_eq!(sources, ["a/one.tfstate", "b/two.tfstate"]);
        assert!(report
            .plan
            .resources
            .iter()
            .all(|r| r.id.contains("aws_s3_bucket.x")));
        assert_eq!(report.counts.changed_keys, 1);
        assert_eq!(report.counts.created_keys, 1);
        assert_eq!(report.plan.summary.add, 1);
        assert_eq!(report.plan.summary.change, 1);
        assert_eq!(
            report.plan.metadata.terraform_version.as_deref(),
            Some("1.9.0")
        );
        assert!(report.plan.source_versions.is_none());
        let sources = report.plan.compact_sources.as_ref().unwrap();
        assert_eq!(sources.create.len(), 1);
        assert_eq!(sources.create[0].key, "b/two.tfstate");
        assert_eq!(sources.create[0].from_version, None);
        assert_eq!(sources.create[0].until_version.as_deref(), Some("v9"));
        assert_eq!(sources.update[0].from_version.as_deref(), Some("v1"));
        assert!(sources.destroy.is_empty());
    }

    #[test]
    fn merge_prefixes_and_dedupes_warnings_and_marks_deleted() {
        let mut destroyed = plan_of(Some(&state(&[("x", "1")])), None);
        destroyed.warnings.push(Warning {
            message: "w".into(),
            level: crate::WarningLevel::Info,
        });
        destroyed.warnings.push(Warning {
            message: "w".into(),
            level: crate::WarningLevel::Info,
        });
        let mut out = outcome(
            "gone.tfstate",
            CompactFileStatus::Deleted,
            Some("v1"),
            None,
            Some(destroyed),
        );
        out.file.until = Some(CompactVersion {
            version_id: Some("m".into()),
            last_modified: Some(ts(2)),
            delete_marker: true,
        });
        let report = report_of(Some(bound(0)), vec![out], vec![]);
        assert_eq!(report.plan.warnings.len(), 1);
        assert_eq!(report.plan.warnings[0].message, "gone.tfstate: w");
        assert_eq!(report.counts.deleted_keys, 1);
        let sources = report.plan.compact_sources.as_ref().unwrap();
        assert_eq!(sources.destroy.len(), 1);
        assert!(sources.destroy[0].deleted);
        assert_eq!(sources.destroy[0].until_version, None);
        assert_eq!(report.plan.metadata.terraform_version, None);
    }

    #[test]
    fn merge_mixed_terraform_versions_yield_none() {
        let a = plan_of(None, Some(&state(&[("x", "1")])));
        let b = plan_of(None, Some(&state(&[("y", "1")]).replace("1.9.0", "1.8.0")));
        let report = report_of(
            None,
            vec![
                outcome("a", CompactFileStatus::Created, None, Some("v1"), Some(a)),
                outcome("b", CompactFileStatus::Created, None, Some("v1"), Some(b)),
            ],
            vec![],
        );
        assert_eq!(report.plan.metadata.terraform_version, None);
    }

    #[test]
    fn refresh_recomputes_after_filtering() {
        let a = plan_of(
            Some(&state(&[("x", "1"), ("y", "1")])),
            Some(&state(&[("x", "2"), ("z", "1")])),
        );
        let mut report = report_of(
            None,
            vec![outcome(
                "a",
                CompactFileStatus::Changed,
                Some("v1"),
                Some("v2"),
                Some(a),
            )],
            vec![],
        );
        assert_eq!(report.plan.summary.add, 1);
        assert_eq!(report.plan.summary.change, 1);
        assert_eq!(report.plan.summary.destroy, 1);
        report
            .plan
            .resources
            .retain(|r| r.action == ActionType::Update);
        refresh_compact_sources(&mut report);
        assert_eq!(report.plan.summary.add, 0);
        assert_eq!(report.plan.summary.destroy, 0);
        assert_eq!(report.plan.summary.change, 1);
        let sources = report.plan.compact_sources.as_ref().unwrap();
        assert!(sources.create.is_empty() && sources.destroy.is_empty());
        assert_eq!(sources.update[0].key, "a");
        // Summary type default check
        let _ = Summary::default();
    }

    #[test]
    fn errors_make_report_incomplete() {
        let report = report_of(None, vec![], vec!["k: boom".into()]);
        assert!(!report.complete);
        assert_eq!(report.counts.errors, 1);
        assert!(render_terminal(&report, no_color(), false).contains("INCOMPLETE"));
    }

    // ---- formatting ----

    fn no_color() -> FormatterOptions {
        FormatterOptions {
            show_resource_identity: true,
            show_full_attribution: false,
            color: false,
        }
    }

    fn sample_report(since: Option<DateTime<Utc>>, key: &str) -> CompactReport {
        let created = plan_of(None, Some(&state(&[("x", "1")])));
        let mut report = report_of(
            since,
            vec![outcome(
                key,
                CompactFileStatus::Created,
                None,
                Some("v1"),
                Some(created),
            )],
            vec![],
        );
        report.files.push(CompactFile {
            key: "notes.txt".into(),
            status: CompactFileStatus::SkippedNonState,
            from: None,
            until: None,
            reason: Some("object contents are not Terraform v4 state JSON".into()),
        });
        report.counts.skipped_non_state = 1;
        report
    }

    #[test]
    fn terminal_has_header_sources_and_resource_source() {
        let report = sample_report(None, "dev/b.tfstate");
        let out = render_terminal(&report, no_color(), false);
        assert!(out.starts_with("S3 compact: 1 matching keys | 0 changed | 1 created"));
        assert!(out.contains("since beginning"));
        assert!(out.contains("Create sources:"));
        assert!(out.contains("dev/b.tfstate  ((none) -> v1)"));
        assert!(out.contains("Source: dev/b.tfstate"));
        assert!(out.contains("notes.txt"));
        assert!(!out.contains("Warning:"));
        let quiet = render_terminal(&report, no_color(), true);
        assert!(!quiet.contains("notes.txt"));
    }

    #[test]
    fn sources_block_marks_deleted() {
        let block = format_sources_block(&CompactSources {
            destroy: vec![SourceRef {
                key: "k".into(),
                from_version: Some("v1".into()),
                until_version: None,
                deleted: true,
            }],
            ..Default::default()
        });
        assert!(block.contains("Destroy sources:"));
        assert!(block.contains("k  (v1 -> (none)) [deleted]"));
        assert!(!block.contains("Create sources"));
    }

    #[test]
    fn since_warning_presence_and_absence_in_all_formats() {
        let with = sample_report(Some(bound(1)), "k");
        let without = sample_report(None, "k");
        assert!(without.since_warning.is_none());
        let warning = with.since_warning.clone().unwrap();
        assert!(warning.starts_with(
            "--compact with --since: only changes after 2026-01-01T01:00:00Z are shown;"
        ));

        assert!(render_terminal(&with, no_color(), false).contains(&format!("Warning: {warning}")));
        assert!(format_compact_markdown_with_options(&with, no_color())
            .contains(&format!("> **Warning:** {warning}")));
        assert!(format_compact_html_with_options(&with, no_color()).contains("compact-warning"));
        assert!(format_compact_summary(&with).contains("Warning: --compact with --since"));
        assert!(format_compact_json(&with)
            .unwrap()
            .contains("\"since_warning\""));

        assert!(!render_terminal(&without, no_color(), false).contains("Warning:"));
        assert!(
            !format_compact_markdown_with_options(&without, no_color()).contains("**Warning:**")
        );
        assert!(!format_compact_html_with_options(&without, no_color())
            .contains("<div class=\"compact-warning\">"));
        assert!(!format_compact_summary(&without).contains("Warning:"));
        assert!(!format_compact_json(&without)
            .unwrap()
            .contains("since_warning"));
    }

    #[test]
    fn terminal_warning_is_yellow_with_color() {
        let report = sample_report(Some(bound(1)), "k");
        let options = FormatterOptions {
            color: true,
            ..no_color()
        };
        let out = render_terminal(&report, options, false);
        colored::control::set_override(false);
        assert!(out.contains("\u{1b}[33mWarning:"));
    }

    #[test]
    fn html_escapes_keys() {
        let report = sample_report(None, "dev/<b>&.tfstate");
        let html = format_compact_html_with_options(&report, no_color());
        assert!(html.contains("dev/&lt;b&gt;&amp;.tfstate"));
        assert!(!html.contains("dev/<b>"));
        assert!(html.contains("class=\"resource-source\""));
    }

    #[test]
    fn markdown_has_source_line_and_sources() {
        let report = sample_report(None, "dev/b.tfstate");
        let md = format_compact_markdown_with_options(&report, no_color());
        assert!(md.contains("- Source: `dev/b.tfstate`"));
        assert!(md.contains("## Create sources"));
    }

    #[test]
    fn json_round_trips_compact_sources() {
        let report = sample_report(None, "dev/b.tfstate");
        let json = format_compact_json(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let sources: CompactSources =
            serde_json::from_value(value["plan"]["compact_sources"].clone()).unwrap();
        assert_eq!(Some(&sources), report.plan.compact_sources.as_ref());
        assert_eq!(value["plan"]["resources"][0]["source"], "dev/b.tfstate");
        assert_eq!(value["files"][1]["status"], "skipped_non_state");
        let _: Option<&Resource> = report.plan.resources.first();
    }

    #[test]
    fn atomic_sources_show_versions_per_resource() {
        let mut report = sample_report(None, "dev/b.tfstate");
        apply_source_display(&mut report, false, true, false);
        let out = render_terminal(&report, no_color(), false);
        assert!(out.contains("Source: dev/b.tfstate  ((none) -> v1)"));
        assert!(out.contains("Create sources:"));

        let mut structured = sample_report(None, "dev/b.tfstate");
        apply_source_display(&mut structured, false, true, true);
        assert_eq!(
            structured.plan.resources[0].source.as_deref(),
            Some("dev/b.tfstate")
        );
    }

    #[test]
    fn quiet_hides_all_sources() {
        let mut report = sample_report(None, "dev/b.tfstate");
        apply_source_display(&mut report, true, true, false);
        assert!(report.plan.compact_sources.is_none());
        let out = render_terminal(&report, no_color(), true);
        assert!(!out.contains("Create sources"));
        assert!(!out.contains("Source:"));
        let md = format_compact_markdown_with_options(&report, no_color());
        assert!(!md.contains("## Create sources"));
        assert!(!md.contains("- Source:"));
        let html = format_compact_html_with_options(&report, no_color());
        assert!(!html.contains("<section class=\"compact-sources\">"));
        assert!(!html.contains("class=\"resource-source\""));
        let json = format_compact_json(&report).unwrap();
        assert!(!json.contains("compact_sources"));
        assert!(!json.contains("\"source\": \"dev/b.tfstate\""));
    }

    #[test]
    fn summary_has_counts() {
        let out = format_compact_summary(&sample_report(None, "k"));
        assert!(out.contains("1 to add, 0 to change, 0 to destroy"));
    }
}
