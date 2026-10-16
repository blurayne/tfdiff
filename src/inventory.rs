use crate::input::{S3Inventory, S3InventoryVersion, S3VersionReader};
use crate::{is_terraform_v4_state, TerraformPlan, TfdiffError};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_english::{parse_duration, Interval};
use chrono_tz::Tz;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::io::IsTerminal;
use std::str::FromStr;

use crate::progress::{ProgressReporter, MAX_CONCURRENT_S3_COMPARISONS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Debug)]
pub struct InventoryOptions {
    pub since: Option<String>,
    pub until: Option<String>,
    pub timezone: Option<String>,
    pub utc: bool,
    pub sort_order: SortOrder,
    pub skip_deleted: bool,
    pub full_inventory: bool,
}

impl Default for InventoryOptions {
    fn default() -> Self {
        Self {
            since: None,
            until: None,
            timezone: None,
            utc: false,
            sort_order: SortOrder::Desc,
            skip_deleted: false,
            full_inventory: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchReport {
    pub source: String,
    pub complete: bool,
    pub counts: BatchCounts,
    pub events: Vec<BatchEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory: Option<Vec<InventorySummary>>,
    pub errors: Vec<String>,
    pub skip_deleted_note: Option<String>,
    /// Resources destroyed in one state file and created in another (see `crate::migrations`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<crate::migrations::StateMigration>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BatchCounts {
    pub matching_keys: usize,
    pub in_range_events: usize,
    pub diffs: usize,
    pub deletions: usize,
    pub skipped_non_state: usize,
    pub expected_skips: usize,
    pub errors: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchEvent {
    pub key: String,
    pub timestamp: String,
    #[serde(skip)]
    history_order: usize,
    pub version_id: Option<String>,
    pub kind: BatchEventKind,
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<TerraformPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_version: Option<String>,
}

#[cfg(test)]
impl BatchEvent {
    pub(crate) fn test_diff(
        key: &str,
        timestamp: &str,
        version: &str,
        plan: TerraformPlan,
    ) -> Self {
        Self {
            key: key.to_string(),
            timestamp: timestamp.to_string(),
            history_order: 0,
            version_id: Some(version.to_string()),
            kind: BatchEventKind::Diff,
            reason: None,
            plan: Some(plan),
            left_version: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchEventKind {
    Diff,
    Deletion,
    Skipped,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventorySummary {
    pub key: String,
    pub last_modified: Option<String>,
    pub status: String,
    pub in_range_events: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TimedEvent {
    pub(crate) key: String,
    pub(crate) history_index: usize,
    pub(crate) event: S3InventoryVersion,
    pub(crate) timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub(crate) struct ComparisonJob {
    pub(crate) order: usize,
    pub(crate) event: TimedEvent,
    pub(crate) timestamp: String,
    pub(crate) version_id: String,
    pub(crate) left_id: String,
    pub(crate) left_etag: Option<String>,
    pub(crate) left_modified: Option<String>,
    pub(crate) right_etag: Option<String>,
    pub(crate) right_modified: Option<String>,
}

#[derive(Clone, Copy)]
enum Bound {
    Since,
    Until,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StateContent {
    TerraformV4,
    Unrelated,
    Invalid(String),
}

pub(crate) fn classify_state_content(contents: &str) -> StateContent {
    let parsed = match serde_json::from_str::<serde_json::Value>(contents) {
        Ok(value) => value,
        Err(_) => {
            return if matches!(contents.trim_start().chars().next(), Some('{' | '[')) {
                StateContent::Invalid("malformed JSON-shaped S3 history entry".to_string())
            } else {
                StateContent::Unrelated
            };
        }
    };
    if is_terraform_v4_state(contents) {
        return StateContent::TerraformV4;
    }
    let looks_like_state = parsed.get("terraform_version").is_some()
        || parsed.get("resources").is_some()
        || parsed.get("lineage").is_some()
        || parsed.get("serial").is_some();
    if looks_like_state {
        StateContent::Invalid(
            "S3 history entry is not a supported Terraform v4 state snapshot".to_string(),
        )
    } else {
        StateContent::Unrelated
    }
}

#[derive(Clone, Copy)]
enum Zone {
    Utc,
    Local,
    Iana(Tz),
    Fixed(chrono::FixedOffset),
}

pub fn validate_options(options: &InventoryOptions) -> crate::Result<()> {
    if options.utc && options.timezone.is_some() {
        return Err(argument_error("--utc cannot be combined with --timezone"));
    }
    if let Some(timezone) = &options.timezone {
        Zone::from_name(timezone)?;
    }
    Ok(())
}

impl Zone {
    fn from_name(name: &str) -> crate::Result<Self> {
        if name.eq_ignore_ascii_case("utc") || name == "Z" {
            return Ok(Self::Utc);
        }
        if let Some(offset) = parse_offset(name) {
            return Ok(Self::Fixed(offset));
        }
        Tz::from_str(name)
            .map(Self::Iana)
            .map_err(|_| argument_error(&format!("unknown IANA time zone: {name}")))
    }
}

fn parse_offset(value: &str) -> Option<chrono::FixedOffset> {
    if value == "Z" || value.eq_ignore_ascii_case("UTC") {
        return chrono::FixedOffset::east_opt(0);
    }
    let raw = value
        .strip_prefix("UTC")
        .or_else(|| value.strip_prefix("GMT"))
        .unwrap_or(value);
    let (sign, digits) = match raw.as_bytes().first()? {
        b'+' => (1, &raw[1..]),
        b'-' => (-1, &raw[1..]),
        _ => return None,
    };
    let normalized = digits.replace(':', "");
    let (hours, minutes) = match normalized.len() {
        2 => (normalized[..2].parse::<i32>().ok()?, 0),
        4 => (
            normalized[..2].parse::<i32>().ok()?,
            normalized[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    chrono::FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

fn resolve_zone(options: &InventoryOptions) -> crate::Result<Zone> {
    if options.utc {
        return Ok(Zone::Utc);
    }
    if let Some(timezone) = &options.timezone {
        return Zone::from_name(timezone);
    }
    Ok(Zone::Local)
}

fn argument_error(message: &str) -> TfdiffError {
    TfdiffError::ParseError(format!("Invalid inventory options: {message}"))
}

fn parse_bound(
    raw: &str,
    bound: Bound,
    default_zone: Zone,
    now: DateTime<Utc>,
) -> crate::Result<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(argument_error("date/time filter must not be empty"));
    }
    if trimmed.eq_ignore_ascii_case("today") {
        return calendar_boundary(now, default_zone, bound);
    }
    if let Some(seconds) = parse_compact_duration(trimmed)? {
        return now
            .checked_sub_signed(Duration::seconds(seconds))
            .ok_or_else(|| argument_error("relative date is outside the supported range"));
    }
    if let Some(relative) = parse_bounded_relative(trimmed)? {
        let relative_time = match relative {
            Interval::Seconds(seconds) => now
                .checked_add_signed(Duration::seconds(i64::from(seconds)))
                .ok_or_else(|| argument_error("relative date is outside the supported range"))?,
            Interval::Days(days) => {
                let base = relative_base(now, default_zone);
                let local = base
                    .checked_add_signed(Duration::days(i64::from(days)))
                    .ok_or_else(|| argument_error("relative date is outside the supported range"))?;
                match default_zone {
                    Zone::Iana(zone) => resolve_timezone(zone, local.naive_local())?,
                    Zone::Local => resolve_local(local.naive_local())?,
                    Zone::Utc | Zone::Fixed(_) => local.with_timezone(&Utc),
                }
            }
            Interval::Months(_) => {
                return Err(argument_error(
                    "relative month/year expressions are not supported; use seconds, minutes, hours, days, or weeks",
                ))
            }
        };
        return Ok(relative_time.with_timezone(&Utc));
    }

    if let Ok(datetime) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(datetime.with_timezone(&Utc));
    }
    if let Some((wall_time, offset)) = split_numeric_suffix(trimmed) {
        if let Some(offset) = parse_offset(offset) {
            return parse_wall_time_fixed(wall_time, offset, bound);
        }
    }
    if let Some((wall_time, zone)) = split_iana_suffix(trimmed) {
        return parse_wall_time(wall_time, zone, bound, now);
    }
    parse_wall_time(trimmed, default_zone, bound, now)
}

fn parse_compact_duration(value: &str) -> crate::Result<Option<i64>> {
    let Some(unit) = value.chars().last() else {
        return Ok(None);
    };
    if value.starts_with('-') && matches!(unit, 'd' | 'h' | 'm' | 's') {
        return Err(argument_error("compact relative duration must be positive"));
    }
    let multiplier = match unit {
        'd' => 24 * 60 * 60,
        'h' => 60 * 60,
        'm' => 60,
        's' => 1,
        _ => return Ok(None),
    };
    let digits = &value[..value.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    let amount = digits
        .parse::<i64>()
        .map_err(|_| argument_error("compact relative duration is too large"))?;
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| argument_error("compact relative duration is too large"))?;
    Ok(Some(seconds))
}

fn parse_bounded_relative(value: &str) -> crate::Result<Option<Interval>> {
    let lower = value.to_ascii_lowercase();
    if lower == "now" {
        return Ok(Some(Interval::Seconds(0)));
    }
    if lower.contains("ago") {
        let words: Vec<_> = lower.split_whitespace().collect();
        if words.len() != 3 || words[2] != "ago" || !words[0].bytes().all(|b| b.is_ascii_digit()) {
            return Err(argument_error(
                "relative filters must use `N seconds|minutes|hours|days|weeks ago`",
            ));
        }
    }
    let words: Vec<_> = lower.split_whitespace().collect();
    if words.len() != 3 || words[2] != "ago" {
        return Ok(None);
    }
    if !words[0].bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    let number = words[0]
        .parse::<i32>()
        .map_err(|_| argument_error("relative interval is too large"))?;
    if number < 0 {
        return Err(argument_error("relative interval must be nonnegative"));
    }
    let unit = words[1].trim_end_matches('s');
    let interval = match unit {
        "second" => Interval::Seconds(
            number
                .checked_neg()
                .ok_or_else(|| argument_error("relative interval is too large"))?,
        ),
        "minute" => Interval::Seconds(
            number
                .checked_mul(-60)
                .ok_or_else(|| argument_error("relative interval is too large"))?,
        ),
        "hour" => Interval::Seconds(
            number
                .checked_mul(-60 * 60)
                .ok_or_else(|| argument_error("relative interval is too large"))?,
        ),
        "day" => Interval::Days(
            number
                .checked_neg()
                .ok_or_else(|| argument_error("relative interval is too large"))?,
        ),
        "week" => Interval::Days(
            number
                .checked_mul(-7)
                .ok_or_else(|| argument_error("relative interval is too large"))?,
        ),
        _ => return Ok(None),
    };
    // parse_duration is used to validate compatibility with the documented grammar.
    let _ = parse_duration(&format!("{number} {} ago", unit))
        .map_err(|_| argument_error("invalid relative interval"))?;
    Ok(Some(interval))
}

fn relative_base(now: DateTime<Utc>, zone: Zone) -> DateTime<chrono::FixedOffset> {
    match zone {
        Zone::Utc => now.with_timezone(&chrono::FixedOffset::east_opt(0).unwrap()),
        Zone::Local => now.with_timezone(Local::now().offset()),
        Zone::Iana(zone) => now.with_timezone(&zone).fixed_offset(),
        Zone::Fixed(offset) => now.with_timezone(&offset),
    }
}

fn calendar_boundary(now: DateTime<Utc>, zone: Zone, bound: Bound) -> crate::Result<DateTime<Utc>> {
    let local = match zone {
        Zone::Utc => now.with_timezone(&Tz::UTC),
        Zone::Fixed(offset) => {
            let local = now.with_timezone(&offset);
            let boundary = match bound {
                Bound::Since => NaiveTime::MIN,
                Bound::Until => NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap(),
            };
            return Ok(offset
                .from_local_datetime(&local.date_naive().and_time(boundary))
                .single()
                .unwrap()
                .with_timezone(&Utc));
        }
        Zone::Local => {
            let now_local = now.with_timezone(&Local);
            let date = now_local.date_naive();
            let boundary = match bound {
                Bound::Since => NaiveTime::MIN,
                Bound::Until => NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap(),
            };
            let naive = date.and_time(boundary);
            let ambiguous = Local.from_local_datetime(&naive);
            let local = match ambiguous {
                chrono::LocalResult::Single(value) => value,
                chrono::LocalResult::Ambiguous(_, _) => {
                    return Err(argument_error("local calendar boundary is ambiguous"))
                }
                chrono::LocalResult::None => {
                    return Err(argument_error("local calendar boundary does not exist"))
                }
            };
            return Ok(local.with_timezone(&Utc));
        }
        Zone::Iana(zone) => now.with_timezone(&zone),
    };
    let date = local.date_naive();
    let boundary = match bound {
        Bound::Since => NaiveTime::MIN,
        Bound::Until => NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap(),
    };
    let naive = date.and_time(boundary);
    let resolved = match local.timezone().from_local_datetime(&naive) {
        chrono::LocalResult::Single(value) => value,
        chrono::LocalResult::Ambiguous(_, _) => {
            return Err(argument_error("local calendar boundary is ambiguous"))
        }
        chrono::LocalResult::None => {
            return Err(argument_error("local calendar boundary does not exist"))
        }
    };
    Ok(resolved.with_timezone(&Utc))
}

fn split_iana_suffix(value: &str) -> Option<(&str, Zone)> {
    let (date, zone) = value.rsplit_once(' ')?;
    if zone.starts_with('+') || zone.starts_with('-') {
        return None;
    }
    let zone = Zone::from_name(zone).ok()?;
    matches!(zone, Zone::Iana(_) | Zone::Utc).then_some((date, zone))
}

fn split_numeric_suffix(value: &str) -> Option<(&str, &str)> {
    let split = value.rfind(char::is_whitespace)?;
    Some((value[..split].trim_end(), value[split..].trim()))
}

fn parse_wall_time(
    raw: &str,
    zone: Zone,
    bound: Bound,
    now: DateTime<Utc>,
) -> crate::Result<DateTime<Utc>> {
    match zone {
        Zone::Utc => parse_wall_time_tz(raw, Tz::UTC, bound, now),
        Zone::Fixed(offset) => parse_wall_time_fixed(raw, offset, bound),
        Zone::Iana(zone) => parse_wall_time_tz(raw, zone, bound, now),
        Zone::Local => {
            if let Ok(date) = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d") {
                let time = match bound {
                    Bound::Since => NaiveTime::MIN,
                    Bound::Until => NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap(),
                };
                return resolve_local(date.and_time(time));
            }
            if let Some(naive) = parse_iso_datetime(raw) {
                return resolve_local(naive);
            }
            let local_now = now.with_timezone(&Local);
            let parsed = chrono_english::parse_date_string(raw, local_now, chrono_english::Dialect::Uk)
                .map_err(|_| argument_error(&format!("could not parse date/time `{raw}`; use YYYY-MM-DD, RFC3339, `today`, or `N days ago`")))?;
            Ok(parsed.with_timezone(&Utc))
        }
    }
}

fn parse_wall_time_tz<TzType: TimeZone>(
    raw: &str,
    zone: TzType,
    bound: Bound,
    now: DateTime<Utc>,
) -> crate::Result<DateTime<Utc>>
where
    TzType::Offset: Copy,
{
    if let Ok(date) = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d") {
        let time = match bound {
            Bound::Since => NaiveTime::MIN,
            Bound::Until => NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap(),
        };
        let datetime = date.and_time(time);
        return resolve_timezone(zone, datetime);
    }
    if let Some(datetime) = parse_iso_datetime(raw) {
        return resolve_timezone(zone, datetime);
    }
    let now_local = now.with_timezone(&zone);
    let parsed = chrono_english::parse_date_string(raw, now_local, chrono_english::Dialect::Uk)
        .map_err(|_| argument_error(&format!("could not parse date/time `{raw}`; use YYYY-MM-DD, RFC3339, `today`, or `N days ago`")))?;
    Ok(parsed.with_timezone(&Utc))
}

fn parse_iso_datetime(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .ok()
}

fn resolve_timezone<TzType: TimeZone>(
    zone: TzType,
    value: NaiveDateTime,
) -> crate::Result<DateTime<Utc>> {
    let resolved = match zone.from_local_datetime(&value) {
        chrono::LocalResult::Single(datetime) => datetime,
        chrono::LocalResult::Ambiguous(_, _) => return Err(argument_error("date/time is ambiguous in the selected time zone; include an explicit UTC offset")),
        chrono::LocalResult::None => return Err(argument_error("date/time does not exist in the selected time zone; choose another time or include an explicit UTC offset")),
    };
    Ok(resolved.with_timezone(&Utc))
}

fn resolve_local(value: NaiveDateTime) -> crate::Result<DateTime<Utc>> {
    let resolved = match Local.from_local_datetime(&value) {
        chrono::LocalResult::Single(datetime) => datetime,
        chrono::LocalResult::Ambiguous(_, _) => return Err(argument_error("date/time is ambiguous in the local time zone; include an explicit UTC offset or use --utc")),
        chrono::LocalResult::None => return Err(argument_error("date/time does not exist in the local time zone; choose another time or use --utc")),
    };
    Ok(resolved.with_timezone(&Utc))
}

fn parse_wall_time_fixed(
    raw: &str,
    offset: chrono::FixedOffset,
    bound: Bound,
) -> crate::Result<DateTime<Utc>> {
    let trimmed = raw.trim();
    let naive = if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        date.and_time(match bound {
            Bound::Since => NaiveTime::MIN,
            Bound::Until => NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap(),
        })
    } else {
        parse_iso_datetime(trimmed)
            .ok_or_else(|| argument_error("a numeric timezone suffix requires an ISO date/time"))?
    };
    Ok(offset
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| argument_error("invalid date with numeric timezone"))?
        .with_timezone(&Utc))
}

type TimeRange = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

pub fn parse_time_bound(
    value: &str,
    is_since: bool,
    options: &InventoryOptions,
    now: DateTime<Utc>,
) -> crate::Result<DateTime<Utc>> {
    validate_options(options)?;
    let zone = resolve_zone(options)?;
    parse_bound(
        value,
        if is_since { Bound::Since } else { Bound::Until },
        zone,
        now,
    )
}

pub fn parse_time_range(
    options: &InventoryOptions,
    now: DateTime<Utc>,
) -> crate::Result<TimeRange> {
    validate_options(options)?;
    let zone = resolve_zone(options)?;
    let since = options
        .since
        .as_deref()
        .map(|value| parse_bound(value, Bound::Since, zone, now))
        .transpose()?;
    let until = options
        .until
        .as_deref()
        .map(|value| parse_bound(value, Bound::Until, zone, now))
        .transpose()?;
    if since.zip(until).is_some_and(|(since, until)| since > until) {
        return Err(argument_error(
            "--since must be earlier than or equal to --until",
        ));
    }
    Ok((since, until))
}

pub async fn build_batch_report(
    source: &S3Inventory,
    options: &InventoryOptions,
) -> crate::Result<BatchReport> {
    build_batch_report_cached(source, options, &crate::input::S3CachePolicy::default()).await
}

pub async fn build_batch_report_cached(
    source: &S3Inventory,
    options: &InventoryOptions,
    cache_policy: &crate::input::S3CachePolicy,
) -> crate::Result<BatchReport> {
    let color = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TFDIFF_NO_COLOR")
            .map(|value| !matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(true);
    build_batch_report_cached_with_options(source, options, cache_policy, false, color).await
}

pub async fn build_batch_report_cached_with_options(
    source: &S3Inventory,
    options: &InventoryOptions,
    cache_policy: &crate::input::S3CachePolicy,
    line_progress: bool,
    color: bool,
) -> crate::Result<BatchReport> {
    build_batch_report_cached_with_options_and_s3_config(
        source,
        options,
        cache_policy,
        &crate::input::S3ClientConfig::new(None),
        line_progress,
        color,
    )
    .await
}

pub async fn build_batch_report_cached_with_options_and_s3_config(
    source: &S3Inventory,
    options: &InventoryOptions,
    cache_policy: &crate::input::S3CachePolicy,
    s3_config: &crate::input::S3ClientConfig,
    line_progress: bool,
    color: bool,
) -> crate::Result<BatchReport> {
    let (since, until) = parse_time_range(options, Utc::now())?;
    let scan = crate::input::list_s3_inventory_with_config(source, s3_config).await?;
    build_batch_report_with(
        source,
        options,
        &scan,
        (since, until),
        cache_policy,
        s3_config,
        (line_progress, color),
    )
    .await
}

async fn build_batch_report_with(
    source: &S3Inventory,
    options: &InventoryOptions,
    scan: &crate::input::S3InventoryScan,
    range: TimeRange,
    cache_policy: &crate::input::S3CachePolicy,
    s3_config: &crate::input::S3ClientConfig,
    progress_options: (bool, bool),
) -> crate::Result<BatchReport> {
    let (since, until) = range;
    let (line_progress, color) = progress_options;
    let objects = &scan.objects;
    let mut events = Vec::new();
    let mut errors = scan.errors.clone();
    let multi_object_read_count = objects
        .iter()
        .flat_map(|object| {
            object
                .history
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    let in_range = event
                        .last_modified
                        .as_deref()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .map(|value| value.with_timezone(&Utc))
                        .is_some_and(|timestamp| {
                            since.is_none_or(|bound| timestamp >= bound)
                                && until.is_none_or(|bound| timestamp <= bound)
                        });
                    (in_range
                        && !event.is_delete_marker
                        && event.version_id.is_some()
                        && object.history.get(index + 1).is_some_and(|predecessor| {
                            !predecessor.is_delete_marker && predecessor.version_id.is_some()
                        }))
                    .then_some(2usize)
                })
        })
        .sum::<usize>();
    let multi_object_comparison_count = multi_object_read_count / 2;
    let version_reader = if multi_object_comparison_count > 0 {
        Some(S3VersionReader::new_with_config(cache_policy, s3_config).await?)
    } else {
        None
    };
    let progress = ProgressReporter::new(
        multi_object_comparison_count,
        "S3 history",
        &format!("reading up to {multi_object_read_count} object versions"),
        line_progress,
        color,
    );
    let mut counts = BatchCounts {
        matching_keys: objects.len(),
        ..Default::default()
    };
    let mut inventory = Vec::new();
    let mut pairs = Vec::with_capacity(multi_object_comparison_count);
    let mut next_comparison_order = 0usize;
    for object in objects {
        let mut timed = Vec::new();
        let latest = object
            .history
            .iter()
            .position(|event| event.is_latest)
            .or_else(|| (!object.history.is_empty()).then_some(0));
        for (history_index, event) in object.history.iter().enumerate() {
            let Some(timestamp) = event
                .last_modified
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))
            else {
                errors.push(format!(
                    "{}: S3 event has no valid LastModified timestamp",
                    object.key
                ));
                continue;
            };
            if since.is_none_or(|bound| timestamp >= bound)
                && until.is_none_or(|bound| timestamp <= bound)
            {
                timed.push(TimedEvent {
                    key: object.key.clone(),
                    history_index,
                    event: event.clone(),
                    timestamp,
                });
            }
        }
        counts.in_range_events += timed.len();
        let current = latest.and_then(|index| object.history.get(index));
        if options.full_inventory {
            inventory.push(InventorySummary {
                key: object.key.clone(),
                last_modified: current.and_then(|entry| entry.last_modified.clone()),
                status: if current.is_some_and(|entry| entry.is_delete_marker) {
                    "deleted".to_string()
                } else {
                    "present".to_string()
                },
                in_range_events: timed.len(),
            });
        }
        for timed_event in timed {
            let timestamp = timed_event
                .timestamp
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
            if timed_event.event.is_delete_marker {
                counts.deletions += 1;
                if options.skip_deleted {
                    continue;
                }
                events.push(BatchEvent {
                    key: timed_event.key,
                    timestamp,
                    history_order: timed_event.history_index,
                    version_id: timed_event.event.version_id,
                    kind: BatchEventKind::Deletion,
                    reason: None,
                    plan: None,
                    left_version: None,
                });
                continue;
            }
            let version_id = timed_event.event.version_id.clone();
            let Some(version_id) = version_id else {
                errors.push(format!(
                    "{}: object version is missing its version ID",
                    timed_event.key
                ));
                continue;
            };
            let predecessor = object.history.get(timed_event.history_index + 1);
            let Some(predecessor) = predecessor else {
                counts.expected_skips += 1;
                events.push(BatchEvent {
                    key: timed_event.key,
                    timestamp,
                    history_order: timed_event.history_index,
                    version_id: Some(version_id),
                    kind: BatchEventKind::Skipped,
                    reason: Some("no immediately preceding S3 history event".to_string()),
                    plan: None,
                    left_version: None,
                });
                continue;
            };
            if predecessor.is_delete_marker {
                counts.expected_skips += 1;
                events.push(BatchEvent {
                    key: timed_event.key,
                    timestamp,
                    history_order: timed_event.history_index,
                    version_id: Some(version_id),
                    kind: BatchEventKind::Skipped,
                    reason: Some(
                        "immediately preceding S3 history event is a delete marker".to_string(),
                    ),
                    plan: None,
                    left_version: predecessor.version_id.clone(),
                });
                continue;
            }
            let Some(left_id) = predecessor.version_id.as_deref() else {
                errors.push(format!(
                    "{}: preceding object version is missing its version ID",
                    timed_event.key
                ));
                continue;
            };
            let right_etag = timed_event.event.etag.clone();
            let right_modified = timed_event.event.last_modified.clone();
            pairs.push(ComparisonJob {
                order: next_comparison_order,
                event: timed_event,
                timestamp,
                version_id,
                left_id: left_id.to_string(),
                left_etag: predecessor.etag.clone(),
                left_modified: predecessor.last_modified.clone(),
                right_etag,
                right_modified,
            });
            next_comparison_order += 1;
        }
    }

    let mut completed_comparisons = 0usize;
    if let Some(reader) = version_reader {
        let run_reader = reader.clone();
        let run_source = source.clone();
        let outcomes = run_ordered_jobs(
            pairs,
            MAX_CONCURRENT_S3_COMPARISONS,
            move |job: ComparisonJob| {
                let reader = run_reader.clone();
                let source = run_source.clone();
                async move { reader.compare_pair(&source, job).await }
            },
            |job: &ComparisonJob| {
                if let Some(progress) = progress.as_ref() {
                    progress.start(&format!("comparison {}", job.event.key));
                }
            },
            |job: &ComparisonJob, task_failed| {
                if let Some(progress) = progress.as_ref() {
                    let detail = if task_failed {
                        "comparison task failed".to_string()
                    } else {
                        format!("comparison {}", job.event.key)
                    };
                    progress.complete(&detail);
                }
            },
        )
        .await;
        completed_comparisons = outcomes.len();

        for outcome in outcomes {
            let job = outcome.job;
            match outcome.result {
                OrderedJobResult::Completed(Ok(Some(plan))) => {
                    counts.diffs += 1;
                    events.push(BatchEvent {
                        key: job.event.key,
                        timestamp: job.timestamp,
                        history_order: job.event.history_index,
                        version_id: Some(job.version_id),
                        kind: BatchEventKind::Diff,
                        reason: None,
                        plan: Some(plan),
                        left_version: Some(job.left_id),
                    });
                }
                OrderedJobResult::Completed(Ok(None)) => {
                    counts.skipped_non_state += 1;
                    events.push(BatchEvent {
                        key: job.event.key,
                        timestamp: job.timestamp,
                        history_order: job.event.history_index,
                        version_id: Some(job.version_id),
                        kind: BatchEventKind::Skipped,
                        reason: Some("object contents are not Terraform v4 state JSON".to_string()),
                        plan: None,
                        left_version: Some(job.left_id),
                    });
                }
                OrderedJobResult::Completed(Err(error)) => {
                    errors.push(format!("{} @{}: {error}", job.event.key, job.version_id));
                }
                OrderedJobResult::TaskFailed(error) => errors.push(format!(
                    "{} @{}: comparison task failed: {error}",
                    job.event.key, job.version_id
                )),
            }
        }
    }
    if let Some(progress) = progress {
        progress.finish(&format!(
            "{completed_comparisons}/{multi_object_comparison_count} comparisons processed"
        ));
    }

    events.sort_by(|left, right| {
        let left_timestamp = DateTime::parse_from_rfc3339(&left.timestamp).ok();
        let right_timestamp = DateTime::parse_from_rfc3339(&right.timestamp).ok();
        let timestamp_order = match (left_timestamp, right_timestamp) {
            (Some(left), Some(right)) => match options.sort_order {
                SortOrder::Asc => left.cmp(&right),
                SortOrder::Desc => right.cmp(&left),
            },
            _ => std::cmp::Ordering::Equal,
        };
        timestamp_order
            .then_with(|| left.key.cmp(&right.key))
            .then_with(|| left.history_order.cmp(&right.history_order))
    });
    counts.errors = errors.len();
    let skipped_deletion_count = if options.skip_deleted {
        counts.deletions
    } else {
        0
    };
    Ok(BatchReport {
        source: format!("s3://{}/{}", source.bucket, source.selector_path()),
        complete: errors.is_empty(),
        counts,
        events,
        inventory: options.full_inventory.then_some(inventory),
        errors,
        skip_deleted_note: (skipped_deletion_count > 0).then(|| {
            format!(
                "Skipped {skipped_deletion_count} deletion events because --skip-deleted was set."
            )
        }),
        migrations: Vec::new(),
    })
}

impl S3Inventory {
    pub(crate) fn selector_path(&self) -> String {
        match &self.selector {
            crate::input::InventorySelector::Prefix(prefix) => prefix.clone(),
            crate::input::InventorySelector::Glob { pattern, .. }
            | crate::input::InventorySelector::Regex { pattern, .. } => pattern.clone(),
        }
    }
}

pub fn format_batch_terminal(report: &BatchReport) -> String {
    format_batch_terminal_with_options(report, crate::formatter::FormatterOptions::default())
}

pub fn format_batch_terminal_with_options(
    report: &BatchReport,
    options: crate::formatter::FormatterOptions,
) -> String {
    format_batch_terminal_with_quiet_options(report, options, false)
}

pub fn format_batch_terminal_with_quiet_options(
    report: &BatchReport,
    options: crate::formatter::FormatterOptions,
    quiet: bool,
) -> String {
    format_batch_terminal_with_display_options(report, options, quiet, false, false)
}

pub fn format_batch_terminal_with_display_options(
    report: &BatchReport,
    options: crate::formatter::FormatterOptions,
    quiet: bool,
    pretty: bool,
    show_datetime: bool,
) -> String {
    let mut output = format!(
        "S3 history: {} matching keys | {} in-range events | {} diffs | {} deletions | {} non-state events skipped | {} expected skips | {} errors{}\n",
        report.counts.matching_keys,
        report.counts.in_range_events,
        report.counts.diffs,
        report.counts.deletions,
        report.counts.skipped_non_state,
        report.counts.expected_skips,
        report.counts.errors,
        if report.complete { "" } else { " | INCOMPLETE INVENTORY" }
    );
    for event in &report.events {
        if quiet
            && matches!(
                event.kind,
                BatchEventKind::Deletion | BatchEventKind::Skipped
            )
        {
            continue;
        }
        if !matches!(event.kind, BatchEventKind::Diff) {
            output.push_str(&format!("\n[{}] {}  ", event.timestamp, event.key));
        }
        match event.kind {
            BatchEventKind::Deletion => output.push_str(&format!(
                "DELETED ({})\n",
                event.version_id.as_deref().unwrap_or("unknown")
            )),
            BatchEventKind::Skipped => output.push_str(&format!(
                "SKIPPED: {}\n",
                event.reason.as_deref().unwrap_or("unspecified")
            )),
            BatchEventKind::Error => output.push_str("ERROR\n"),
            BatchEventKind::Diff => {
                if let Some(plan) = &event.plan {
                    let bucket = report
                        .source
                        .strip_prefix("s3://")
                        .and_then(|source| source.split_once('/'))
                        .map(|(bucket, _)| bucket)
                        .unwrap_or_default();
                    let uri = format!("s3://{bucket}/{}", event.key);
                    let moves = crate::migrations::moves_for_event(&report.migrations, event);
                    output.push_str(
                        &crate::formatter::format_terminal_output_with_s3_provenance_moves(
                            plan,
                            options,
                            Some(&uri),
                            pretty,
                            show_datetime,
                            Some(&moves),
                        ),
                    );
                }
            }
        }
    }
    if let Some(note) = &report.skip_deleted_note {
        output.push_str(&format!("\n{note}\n"));
    }
    if !report.errors.is_empty() {
        output.push_str("\nErrors:\n");
        for error in &report.errors {
            output.push_str(&format!("- {error}\n"));
        }
    }
    output
}

pub fn format_batch_markdown(report: &BatchReport) -> String {
    format_batch_markdown_with_options(report, crate::formatter::FormatterOptions::default())
}

pub fn format_batch_markdown_with_options(
    report: &BatchReport,
    options: crate::formatter::FormatterOptions,
) -> String {
    let mut output = format!(
        "# S3 State History\n\nSource: `{}`  \nMatching keys: {} · In-range events: {} · Diffs: {} · Deletions: {} · Errors: {}\n",
        report.source, report.counts.matching_keys, report.counts.in_range_events,
        report.counts.diffs, report.counts.deletions, report.counts.errors
    );
    for event in &report.events {
        output.push_str(&format!(
            "\n## {} — {}\n\n`{}` · `{}`\n\n",
            event.timestamp,
            event.key,
            event.version_id.as_deref().unwrap_or("none"),
            match event.kind {
                BatchEventKind::Diff => "diff",
                BatchEventKind::Deletion => "deleted",
                BatchEventKind::Skipped => "skipped",
                BatchEventKind::Error => "error",
            }
        ));
        if let Some(plan) = &event.plan {
            let moves = crate::migrations::moves_for_event(&report.migrations, event);
            output.push_str(&crate::formatter::format_markdown_output_with_moves(
                plan,
                options,
                Some(&moves),
            ));
            output.push('\n');
        }
        if let Some(reason) = &event.reason {
            output.push_str(&format!("{}\n", reason));
        }
    }
    if let Some(inventory) = &report.inventory {
        output.push_str("\n## Inventory\n\n| Key | Latest event | Status | In-range events |\n|---|---|---|---:|\n");
        for entry in inventory {
            output.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                entry.key,
                entry.last_modified.as_deref().unwrap_or("unknown"),
                entry.status,
                entry.in_range_events
            ));
        }
    }
    if !report.errors.is_empty() {
        output.push_str("\n## Errors\n");
        for error in &report.errors {
            output.push_str(&format!("\n- {error}"));
        }
        output.push('\n');
    }
    if let Some(note) = &report.skip_deleted_note {
        output.push_str(&format!("\n{note}\n"));
    }
    output
}

pub fn format_batch_json(report: &BatchReport) -> crate::Result<String> {
    serde_json::to_string_pretty(report).map_err(TfdiffError::from)
}

pub fn format_batch_html(report: &BatchReport) -> String {
    format_batch_html_with_options(report, crate::formatter::FormatterOptions::default())
}

pub fn format_batch_html_with_options(
    report: &BatchReport,
    options: crate::formatter::FormatterOptions,
) -> String {
    let mut html = crate::formatter::html::html_document_start("S3 Terraform history");
    html.push_str(&format!("<h1>S3 Terraform history</h1><div class=\"batch-summary\">{} matching keys · {} in-range events · {} diffs · {} deletions · {} errors{}</div>", report.counts.matching_keys, report.counts.in_range_events, report.counts.diffs, report.counts.deletions, report.counts.errors, if report.complete { "" } else { " · inventory incomplete" }));
    for (index, event) in report.events.iter().enumerate() {
        let kind = match event.kind {
            BatchEventKind::Diff => "Diff",
            BatchEventKind::Deletion => "Deleted",
            BatchEventKind::Skipped => "Skipped",
            BatchEventKind::Error => "Error",
        };
        html.push_str(&format!("<details class=\"batch-event\"{}><summary><time>{}</time> · <code>{}</code> · {}</summary><div class=\"batch-meta\">Current version <code>{}</code>{}</div>", if index == 0 { " open" } else { "" }, escape_html(&event.timestamp), escape_html(&event.key), kind, escape_html(event.version_id.as_deref().unwrap_or("unknown")), event.left_version.as_deref().map(|version| format!("<br>Compared with previous version <code>{}</code>", escape_html(version))).unwrap_or_default()));
        if let Some(plan) = &event.plan {
            html.push_str("<div class=\"batch-diff\">");
            let moves = crate::migrations::moves_for_event(&report.migrations, event);
            html.push_str(&crate::formatter::html::render_html_fragment_with_moves(
                plan,
                options,
                Some(&moves),
            ));
            html.push_str("</div>");
        }
        if let Some(reason) = &event.reason {
            html.push_str(&format!(
                "<div class=\"batch-meta\">{}</div>",
                escape_html(reason)
            ));
        }
        html.push_str("</details>");
    }
    if let Some(inventory) = &report.inventory {
        html.push_str("<h2>Full inventory</h2><table class=\"batch-inventory\"><thead><tr><th>Key</th><th>Latest modification</th><th>Status</th><th>In-range events</th></tr></thead><tbody>");
        for entry in inventory {
            html.push_str(&format!(
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&entry.key),
                escape_html(entry.last_modified.as_deref().unwrap_or("unknown")),
                escape_html(&entry.status),
                entry.in_range_events
            ));
        }
        html.push_str("</tbody></table>");
    }
    if let Some(note) = &report.skip_deleted_note {
        html.push_str(&format!("<p>{}</p>", escape_html(note)));
    }
    if !report.errors.is_empty() {
        html.push_str("<h2 class=\"batch-error\">Errors</h2><ul>");
        for error in &report.errors {
            html.push_str(&format!("<li>{}</li>", escape_html(error)));
        }
        html.push_str("</ul>");
    }
    html.push_str("</main></body></html>");
    html
}

fn concise_panic_message(message: &str) -> String {
    message
        .split_once(" panicked with message ")
        .and_then(|(_, message)| serde_json::from_str::<String>(message).ok())
        .unwrap_or_else(|| message.to_string())
}

pub(crate) trait OrderedJob: Clone + Send + 'static {
    fn order(&self) -> usize;
}

impl OrderedJob for ComparisonJob {
    fn order(&self) -> usize {
        self.order
    }
}

#[derive(Debug)]
pub(crate) struct OrderedJobOutcome<J, T> {
    pub(crate) job: J,
    pub(crate) result: OrderedJobResult<T>,
}

#[derive(Debug)]
pub(crate) enum OrderedJobResult<T> {
    Completed(T),
    TaskFailed(String),
}

pub(crate) async fn run_ordered_jobs<J, T, F, Fut, OnStart, OnComplete>(
    jobs: Vec<J>,
    max_concurrent: usize,
    run: F,
    mut on_start: OnStart,
    mut on_complete: OnComplete,
) -> Vec<OrderedJobOutcome<J, T>>
where
    J: OrderedJob,
    T: Send + 'static,
    F: Fn(J) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = T> + Send + 'static,
    OnStart: FnMut(&J),
    OnComplete: FnMut(&J, bool),
{
    if jobs.is_empty() {
        return Vec::new();
    }
    assert!(max_concurrent > 0, "comparison concurrency must be nonzero");

    let mut tasks = tokio::task::JoinSet::new();
    let mut jobs = jobs.into_iter();
    let mut job_by_task = HashMap::with_capacity(max_concurrent);
    let mut outcomes = Vec::new();
    let run = std::sync::Arc::new(run);

    let spawn =
        |tasks: &mut tokio::task::JoinSet<(tokio::task::Id, T)>, job: J, run: std::sync::Arc<F>| {
            let task_job = job.clone();
            let task_id = tasks.spawn(async move { (tokio::task::id(), run(task_job).await) });
            (task_id, job)
        };

    while tasks.len() < max_concurrent {
        let Some(job) = jobs.next() else {
            break;
        };
        on_start(&job);
        let (task_handle, job) = spawn(&mut tasks, job, run.clone());
        job_by_task.insert(task_handle.id(), job);
    }

    while let Some(result) = tasks.join_next_with_id().await {
        let (task_id, value) = match result {
            Ok((task_id, value)) => (task_id, Ok(value)),
            Err(error) => (error.id(), Err(error.to_string())),
        };
        let job = job_by_task
            .remove(&task_id)
            .expect("every comparison task is associated with a scheduled job");
        let task_failed = value.is_err();
        on_complete(&job, task_failed);
        let result = match value {
            Ok((_, value)) => OrderedJobResult::Completed(value),
            Err(error) => OrderedJobResult::TaskFailed(concise_panic_message(&error)),
        };
        outcomes.push(OrderedJobOutcome { job, result });

        if let Some(job) = jobs.next() {
            on_start(&job);
            let (task_handle, job) = spawn(&mut tasks, job, run.clone());
            job_by_task.insert(task_handle.id(), job);
        }
    }

    outcomes.sort_by_key(|outcome| outcome.job.order());
    outcomes
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn comparison_runner_orders_errors_and_counts_panicked_jobs() {
        let jobs = vec![
            comparison_job(0, "first.tfstate", "first-version"),
            comparison_job(1, "second.tfstate", "second-version"),
            comparison_job(2, "third.tfstate", "third-version"),
        ];
        let outcomes = run_ordered_jobs(
            jobs,
            2,
            |job| async move {
                match job.order {
                    0 => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Err::<(), _>("first failed".to_string())
                    }
                    1 => panic!("simulated panic"),
                    _ => {
                        tokio::task::yield_now().await;
                        Err("third failed".to_string())
                    }
                }
            },
            |job| {
                assert!(job.order < 3);
            },
            |job, _| {
                assert!(job.order < 3);
            },
        )
        .await;

        assert_eq!(outcomes.len(), 3);
        assert_eq!(
            outcomes
                .into_iter()
                .map(|outcome| {
                    let prefix = format!("{} @{}: ", outcome.job.event.key, outcome.job.version_id);
                    match outcome.result {
                        OrderedJobResult::Completed(Err(error)) => format!("{prefix}{error}"),
                        OrderedJobResult::TaskFailed(error) => {
                            format!("{prefix}task failed: {}", concise_panic_message(&error))
                        }
                        _ => panic!("expected each job to fail"),
                    }
                })
                .collect::<Vec<_>>(),
            vec![
                "first.tfstate @first-version: first failed",
                "second.tfstate @second-version: task failed: simulated panic",
                "third.tfstate @third-version: third failed",
            ]
        );
    }

    #[tokio::test]
    async fn comparison_runner_respects_worker_limit_and_completes_every_attempt() {
        let jobs = (0..7)
            .map(|order| {
                comparison_job(
                    order,
                    &format!("state-{order}.tfstate"),
                    &format!("version-{order}"),
                )
            })
            .collect();
        let active = std::sync::Arc::new(AtomicUsize::new(0));
        let max_active = std::sync::Arc::new(AtomicUsize::new(0));
        let starts = std::sync::Arc::new(AtomicUsize::new(0));
        let completions = std::sync::Arc::new(AtomicUsize::new(0));

        let run_active = active.clone();
        let run_max_active = max_active.clone();
        let start_count = starts.clone();
        let completion_count = completions.clone();
        let outcomes = run_ordered_jobs(
            jobs,
            2,
            move |job| {
                let active = run_active.clone();
                let max_active = run_max_active.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                    max_active.fetch_max(current, Ordering::Relaxed);
                    for _ in 0..job.order % 3 + 1 {
                        tokio::task::yield_now().await;
                    }
                    active.fetch_sub(1, Ordering::Relaxed);
                    job.order
                }
            },
            move |_| {
                start_count.fetch_add(1, Ordering::Relaxed);
            },
            move |_, _| {
                completion_count.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await;

        assert_eq!(outcomes.len(), 7);
        assert!(max_active.load(Ordering::Relaxed) <= 2);
        assert_eq!(starts.load(Ordering::Relaxed), 7);
        assert_eq!(completions.load(Ordering::Relaxed), 7);
        assert_eq!(
            outcomes
                .into_iter()
                .map(|outcome| match outcome.result {
                    OrderedJobResult::Completed(order) => order,
                    OrderedJobResult::TaskFailed(error) => panic!("unexpected failure: {error}"),
                })
                .collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
    }

    fn comparison_job(order: usize, key: &str, version_id: &str) -> ComparisonJob {
        ComparisonJob {
            order,
            event: TimedEvent {
                key: key.to_string(),
                history_index: order,
                event: S3InventoryVersion {
                    version_id: Some(version_id.to_string()),
                    last_modified: Some("2026-01-01T00:00:00Z".to_string()),
                    is_delete_marker: false,
                    is_latest: order == 0,
                    size: Some(1),
                    etag: None,
                },
                timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            version_id: version_id.to_string(),
            left_id: format!("{version_id}-previous"),
            left_etag: None,
            left_modified: None,
            right_etag: None,
            right_modified: None,
        }
    }

    fn options(since: Option<&str>, until: Option<&str>) -> InventoryOptions {
        InventoryOptions {
            since: since.map(str::to_string),
            until: until.map(str::to_string),
            utc: true,
            ..Default::default()
        }
    }

    #[test]
    fn classifies_unrelated_malformed_and_unsupported_state_data() {
        assert_eq!(
            classify_state_content("ordinary text"),
            StateContent::Unrelated
        );
        assert!(matches!(
            classify_state_content("{\"version\":4,"),
            StateContent::Invalid(_)
        ));
        assert!(matches!(
            classify_state_content(
                "{\"version\":3,\"terraform_version\":\"1.9.0\",\"resources\":[]}"
            ),
            StateContent::Invalid(_)
        ));
    }

    #[test]
    fn parses_utc_ranges_and_full_date_until() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (since, until) =
            parse_time_range(&options(Some("2026-10-10"), Some("2026-10-11")), now).unwrap();
        assert_eq!(since.unwrap().to_rfc3339(), "2026-10-10T00:00:00+00:00");
        assert_eq!(
            until.unwrap().format("%Y-%m-%dT%H:%M:%S%.fZ").to_string(),
            "2026-10-11T23:59:59.999999999Z"
        );
    }

    #[test]
    fn compact_durations_are_relative_to_injected_now() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for (raw, expected) in [
            ("3d", "2026-10-10T13:45:00Z"),
            ("12h", "2026-10-13T01:45:00Z"),
            ("30m", "2026-10-13T13:15:00Z"),
            ("45s", "2026-10-13T13:44:15Z"),
        ] {
            assert_eq!(
                parse_time_bound(raw, true, &options(None, None), now)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                expected
            );
        }
        assert!(parse_time_bound("0h", true, &options(None, None), now).is_ok());
        assert!(parse_time_bound("-2h", true, &options(None, None), now).is_err());
        assert!(
            parse_time_bound("999999999999999999999d", true, &options(None, None), now).is_err()
        );
    }

    #[test]
    fn supports_bounded_relative_phrases_and_rejects_reversed_ranges() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (since, until) =
            parse_time_range(&options(Some("5 days ago"), Some("today")), now).unwrap();
        assert_eq!(
            since.unwrap(),
            DateTime::parse_from_rfc3339("2026-10-08T13:45:00Z").unwrap()
        );
        assert_eq!(until.unwrap().format("%Y-%m-%d").to_string(), "2026-10-13");
        assert!(parse_time_range(&options(Some("2026-10-12"), Some("2026-10-10")), now).is_err());
    }

    #[test]
    fn date_suffix_and_iana_zone_are_supported_and_dst_gaps_rejected() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let options = InventoryOptions {
            since: Some("2026-10-10 12:00 America/Los_Angeles".into()),
            ..Default::default()
        };
        let (since, _) = parse_time_range(&options, now).unwrap();
        assert_eq!(
            since.unwrap(),
            DateTime::parse_from_rfc3339("2026-10-10T19:00:00Z").unwrap()
        );
        let options = InventoryOptions {
            since: Some("2026-03-08 02:30 America/Los_Angeles".into()),
            ..Default::default()
        };
        assert!(parse_time_range(&options, now).is_err());
    }

    #[test]
    fn supports_numeric_timezone_option_and_space_separated_datetime() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let options = InventoryOptions {
            since: Some("2026-10-10 12:00".into()),
            until: Some("2026-10-10T13:00".into()),
            timezone: Some("-07:00".into()),
            ..Default::default()
        };
        let (since, until) = parse_time_range(&options, now).unwrap();
        assert_eq!(
            since.unwrap(),
            DateTime::parse_from_rfc3339("2026-10-10T19:00:00Z").unwrap()
        );
        assert_eq!(
            until.unwrap(),
            DateTime::parse_from_rfc3339("2026-10-10T20:00:00Z").unwrap()
        );
    }

    #[test]
    fn local_relative_arithmetic_uses_captured_instant_offset() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let options = InventoryOptions {
            since: Some("1 hour ago".into()),
            timezone: Some("America/Los_Angeles".into()),
            ..Default::default()
        };
        let (since, _) = parse_time_range(&options, now).unwrap();
        assert_eq!(
            since.unwrap(),
            DateTime::parse_from_rfc3339("2026-10-13T12:45:00Z").unwrap()
        );
    }

    #[test]
    fn offset_suffix_takes_precedence_over_utc_default() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (since, _) =
            parse_time_range(&options(Some("2026-10-10T12:00:00-07:00"), None), now).unwrap();
        assert_eq!(
            since.unwrap(),
            DateTime::parse_from_rfc3339("2026-10-10T19:00:00Z").unwrap()
        );
    }
}
