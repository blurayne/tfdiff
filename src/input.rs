use crate::inventory::{classify_state_content, ComparisonJob, StateContent};
use crate::{Result, TfdiffError};
use aws_sdk_s3::{
    operation::list_object_versions::ListObjectVersionsOutput,
    types::{BucketVersioningStatus, EncodingType},
    Client,
};
use aws_smithy_runtime_api::http::Response as SmithyResponse;
use aws_smithy_types::date_time::Format;
use globset::{GlobBuilder, GlobMatcher};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct S3ClientConfig {
    region_override: Option<String>,
    sdk_config: std::sync::Arc<tokio::sync::OnceCell<aws_config::SdkConfig>>,
    bucket_clients: std::sync::Arc<tokio::sync::Mutex<HashMap<String, Client>>>,
}

impl S3ClientConfig {
    pub fn new(region_override: Option<String>) -> Self {
        Self {
            region_override,
            sdk_config: std::sync::Arc::new(tokio::sync::OnceCell::new()),
            bucket_clients: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn sdk_config(&self) -> aws_config::SdkConfig {
        self.sdk_config
            .get_or_init(|| async {
                let loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
                match &self.region_override {
                    Some(region) => {
                        loader
                            .region(aws_sdk_s3::config::Region::new(region.clone()))
                            .load()
                            .await
                    }
                    None => loader.load().await,
                }
            })
            .await
            .clone()
    }

    async fn client_for_bucket(&self, bucket: &str) -> Result<Client> {
        let sdk_config = self.sdk_config().await;
        let mut clients = self.bucket_clients.lock().await;
        if let Some(client) = clients.get(bucket) {
            return Ok(client.clone());
        }

        let client = if sdk_config.region().is_some() {
            Client::new(&sdk_config)
        } else {
            let probe_config = s3_builder_for_config(&sdk_config)
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .build();
            let probe_client = Client::from_conf(probe_config);
            let probe = probe_client.head_bucket().bucket(bucket).send().await;
            let region = match probe {
                Ok(output) => output.bucket_region().map(str::to_string),
                Err(error) => error
                    .raw_response()
                    .and_then(|response| response.headers().get("x-amz-bucket-region"))
                    .map(str::to_string),
            }
            .filter(|region| valid_s3_region(region));
            let Some(region) = region else {
                return Err(input_error(
                    &format!("s3://{bucket}"),
                    "could not determine the bucket region from HeadBucket; configure --aws-region or AWS_REGION",
                ));
            };
            let config = s3_builder_for_config(&sdk_config)
                .region(aws_sdk_s3::config::Region::new(region))
                .build();
            Client::from_conf(config)
        };
        clients.insert(bucket.to_string(), client.clone());
        Ok(client)
    }
}

fn s3_builder_for_config(config: &aws_config::SdkConfig) -> aws_sdk_s3::config::Builder {
    let mut builder = aws_sdk_s3::config::Builder::from(config);
    if let Some(endpoint_url) = config.endpoint_url() {
        builder = builder.endpoint_url(endpoint_url);
    }
    if let Some(retry_config) = config.retry_config() {
        builder = builder.retry_config(retry_config.clone());
    }
    builder
}

fn valid_s3_region(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 128
        && region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[derive(Clone, Debug)]
pub struct S3CachePolicy {
    cache: Option<crate::cache::Cache>,
}

impl S3CachePolicy {
    pub fn from_options(no_cache: bool) -> Result<Self> {
        let env_disables_cache = if no_cache {
            false
        } else {
            match std::env::var("TFDIFF_NO_CACHE") {
                Ok(value) => parse_boolean_env(&value).ok_or_else(|| {
                    input_error(
                        "TFDIFF_NO_CACHE",
                        "expected true/false, 1/0, yes/no, or on/off",
                    )
                })?,
                Err(std::env::VarError::NotPresent) => false,
                Err(std::env::VarError::NotUnicode(_)) => true,
            }
        };
        let enabled = !no_cache && !env_disables_cache;
        Ok(Self {
            cache: enabled.then(crate::cache::Cache::discover).flatten(),
        })
    }

    pub fn cache(&self) -> Option<&crate::cache::Cache> {
        self.cache.as_ref()
    }
}

impl Default for S3CachePolicy {
    fn default() -> Self {
        Self::from_options(false).unwrap_or(Self { cache: None })
    }
}

fn parse_boolean_env(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InputSource {
    Local(PathBuf),
    S3(S3Object),
    S3Inventory(S3Inventory),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct S3Inventory {
    pub bucket: String,
    pub selector: InventorySelector,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InventorySelector {
    Prefix(String),
    Glob {
        prefix: String,
        pattern: String,
    },
    /// Anchored regular expression matched against the full object key
    /// (`--regex`). `prefix` is the literal key text listed with ListObjectVersions.
    Regex {
        prefix: String,
        pattern: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedInput {
    pub contents: String,
    pub version: Option<crate::SourceVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3InventoryObject {
    pub key: String,
    pub history: Vec<S3InventoryVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3InventoryVersion {
    pub version_id: Option<String>,
    pub last_modified: Option<String>,
    pub is_delete_marker: bool,
    pub is_latest: bool,
    pub size: Option<i64>,
    pub etag: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3InventoryScan {
    pub objects: Vec<S3InventoryObject>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct S3Object {
    pub bucket: String,
    pub key: String,
    pub selector: VersionSelector,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VersionSelector {
    Current,
    Version(String),
    HeadOffset(usize),
}

impl InputSource {
    pub fn is_s3_uri(path: &Path) -> bool {
        path.to_str()
            .is_some_and(|value| value.starts_with("s3://"))
    }

    pub fn parse(path: &Path) -> Result<Self> {
        Self::parse_with_mode(path, crate::pattern::PatternMode::Glob)
    }

    /// Parses an input path; S3 key patterns are globs or regular expressions
    /// depending on `mode`.
    pub fn parse_with_mode(path: &Path, mode: crate::pattern::PatternMode) -> Result<Self> {
        let Some(value) = path.to_str() else {
            return Ok(Self::Local(path.to_path_buf()));
        };
        if !value.starts_with("s3://") {
            return Ok(Self::Local(path.to_path_buf()));
        }
        parse_s3_source(value, mode)
    }

    pub fn description(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::S3(object) => format!("s3://{}/{}", object.bucket, object.key),
            Self::S3Inventory(inventory) => match &inventory.selector {
                InventorySelector::Prefix(prefix) => {
                    format!("s3://{}/{}", inventory.bucket, prefix)
                }
                InventorySelector::Glob { pattern, .. }
                | InventorySelector::Regex { pattern, .. } => {
                    format!("s3://{}/{}", inventory.bucket, pattern)
                }
            },
        }
    }

    pub fn is_inventory(&self) -> bool {
        matches!(self, Self::S3Inventory(_))
    }
}

fn parse_s3_source(uri: &str, mode: crate::pattern::PatternMode) -> Result<InputSource> {
    let rest = uri.strip_prefix("s3://").unwrap_or_default();
    let (bucket, raw_object) = rest
        .split_once('/')
        .ok_or_else(|| input_error(uri, "expected s3://bucket/key"))?;
    if bucket.is_empty() {
        return Err(input_error(uri, "bucket must not be empty"));
    }
    let decoded = percent_decode(raw_object).map_err(|message| input_error(uri, &message))?;
    let regex_mode = mode == crate::pattern::PatternMode::Regex;
    let is_pattern = if regex_mode {
        contains_regex_syntax(&decoded)
    } else {
        contains_raw_glob(raw_object)
    };
    if is_pattern {
        if raw_object
            .rsplit_once('@')
            .is_some_and(|(_, selector)| !selector.is_empty() && !selector.contains('/'))
        {
            return Err(input_error(
                uri,
                "version selectors are not supported for S3 prefix or glob inputs; encode a literal @ as %40",
            ));
        }
        if regex_mode {
            crate::pattern::anchored_regex(&decoded)
                .map_err(|error| input_error(uri, &format!("invalid S3 regex pattern: {error}")))?;
            return Ok(InputSource::S3Inventory(S3Inventory {
                bucket: bucket.to_string(),
                selector: InventorySelector::Regex {
                    prefix: regex_literal_prefix(&decoded),
                    pattern: decoded,
                },
            }));
        }
        let (prefix, pattern) =
            split_glob_prefix(raw_object).map_err(|message| input_error(uri, &message))?;
        return Ok(InputSource::S3Inventory(S3Inventory {
            bucket: bucket.to_string(),
            selector: InventorySelector::Glob { prefix, pattern },
        }));
    }
    if raw_object.is_empty() || raw_object.ends_with('/') {
        return Ok(InputSource::S3Inventory(S3Inventory {
            bucket: bucket.to_string(),
            selector: InventorySelector::Prefix(decoded),
        }));
    }
    Ok(InputSource::S3(parse_s3_uri(uri)?))
}

const REGEX_SYNTAX: &[char] = &[
    '.', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '^', '$', '\\',
];

/// A `.` alone does not make a key a regex, so plain keys such as
/// `env/app.tfstate` stay exact objects in `--regex` mode.
fn contains_regex_syntax(key: &str) -> bool {
    key.chars().any(|c| c != '.' && REGEX_SYNTAX.contains(&c))
}

/// Literal text before the first regex metacharacter, cut back to the last `/`.
fn regex_literal_prefix(pattern: &str) -> String {
    let first = pattern.find(REGEX_SYNTAX).unwrap_or(pattern.len());
    pattern[..first]
        .rfind('/')
        .map_or_else(String::new, |index| pattern[..=index].to_string())
}

fn contains_raw_glob(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len()
                && hex_value(bytes[index + 1]).is_some()
                && hex_value(bytes[index + 2]).is_some() =>
            {
                index += 3
            }
            b'*' | b'?' | b'{' | b'}' | b'[' | b']' => return true,
            _ => index += 1,
        }
    }
    false
}

fn split_glob_prefix(raw_pattern: &str) -> std::result::Result<(String, String), String> {
    let bytes = raw_pattern.as_bytes();
    let mut index = 0;
    let mut first_glob = None;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len()
                && hex_value(bytes[index + 1]).is_some()
                && hex_value(bytes[index + 2]).is_some() =>
            {
                index += 3;
            }
            b'*' | b'?' | b'{' | b'}' | b'[' | b']' => {
                first_glob = Some(index);
                break;
            }
            _ => index += 1,
        }
    }
    let first_glob = first_glob.ok_or_else(|| "glob selector has no pattern syntax".to_string())?;
    let prefix_end = raw_pattern[..first_glob]
        .rfind('/')
        .map_or(0, |index| index + 1);
    let prefix = percent_decode(&raw_pattern[..prefix_end])?;
    let pattern = decode_glob_pattern(raw_pattern)?;
    Ok((prefix, pattern))
}

fn decode_glob_pattern(raw_pattern: &str) -> std::result::Result<String, String> {
    let bytes = raw_pattern.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut percent_encoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("invalid percent escape in S3 URI".to_string());
            }
            let hi = hex_value(bytes[index + 1])
                .ok_or_else(|| "invalid percent escape in S3 URI".to_string())?;
            let lo = hex_value(bytes[index + 2])
                .ok_or_else(|| "invalid percent escape in S3 URI".to_string())?;
            decoded.push((hi << 4) | lo);
            percent_encoded.push(true);
            index += 3;
        } else {
            let character = raw_pattern[index..]
                .chars()
                .next()
                .ok_or_else(|| "S3 URI contains invalid UTF-8".to_string())?;
            let mut buffer = [0; 4];
            let character_bytes = character.encode_utf8(&mut buffer).as_bytes();
            decoded.extend_from_slice(character_bytes);
            percent_encoded.extend(std::iter::repeat_n(false, character_bytes.len()));
            index += character.len_utf8();
        }
    }
    let decoded =
        String::from_utf8(decoded).map_err(|_| "S3 URI contains invalid UTF-8".to_string())?;
    let mut pattern = String::with_capacity(decoded.len());
    for (byte_index, character) in decoded.char_indices() {
        if percent_encoded[byte_index]
            && matches!(character, '*' | '?' | '{' | '}' | '[' | ']' | '\\')
        {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    Ok(pattern)
}

fn parse_s3_uri(uri: &str) -> Result<S3Object> {
    let rest = uri.strip_prefix("s3://").unwrap_or_default();
    let (bucket, raw_object) = rest
        .split_once('/')
        .ok_or_else(|| input_error(uri, "expected s3://bucket/key"))?;
    if bucket.is_empty() {
        return Err(input_error(uri, "bucket must not be empty"));
    }
    let (raw_key, raw_selector) = match raw_object.rsplit_once('@') {
        Some((key, selector)) => (key, Some(selector)),
        None => (raw_object, None),
    };
    let key = percent_decode(raw_key).map_err(|message| input_error(uri, &message))?;
    if key.is_empty() {
        return Err(input_error(uri, "object key must not be empty"));
    }
    let selector = match raw_selector {
        None => VersionSelector::Current,
        Some("") => return Err(input_error(uri, "version selector must not be empty")),
        Some("HEAD") => VersionSelector::HeadOffset(0),
        Some(raw) if raw.starts_with("HEAD~") => {
            let offset = &raw[5..];
            if offset.is_empty() || !offset.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(input_error(
                    uri,
                    "HEAD offset must be a nonnegative decimal integer",
                ));
            }
            VersionSelector::HeadOffset(
                offset
                    .parse::<usize>()
                    .map_err(|_| input_error(uri, "HEAD offset is too large"))?,
            )
        }
        Some(raw) => VersionSelector::Version(
            percent_decode(raw).map_err(|message| input_error(uri, &message))?,
        ),
    };
    Ok(S3Object {
        bucket: bucket.to_string(),
        key,
        selector,
    })
}

fn percent_decode(raw: &str) -> std::result::Result<String, String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("invalid percent escape in S3 URI".to_string());
            }
            let hi = hex_value(bytes[i + 1])
                .ok_or_else(|| "invalid percent escape in S3 URI".to_string())?;
            let lo = hex_value(bytes[i + 2])
                .ok_or_else(|| "invalid percent escape in S3 URI".to_string())?;
            decoded.push((hi << 4) | lo);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "S3 URI contains invalid UTF-8".to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn input_error(source: &str, message: &str) -> TfdiffError {
    TfdiffError::InputError {
        input: source.to_string(),
        message: message.to_string(),
    }
}

pub async fn read_inputs(sources: &[InputSource]) -> Result<Vec<String>> {
    Ok(load_inputs(sources)
        .await?
        .into_iter()
        .map(|input| input.contents)
        .collect())
}

pub async fn list_s3_inventory(source: &S3Inventory) -> Result<S3InventoryScan> {
    list_s3_inventory_with_config(source, &S3ClientConfig::new(None)).await
}

pub async fn list_s3_inventory_with_config(
    source: &S3Inventory,
    config: &S3ClientConfig,
) -> Result<S3InventoryScan> {
    let client = config.client_for_bucket(&source.bucket).await?;
    list_s3_inventory_uncached(source, &client).await
}

pub async fn list_s3_key_versions_with_config(
    bucket: &str,
    key: &str,
    config: &S3ClientConfig,
) -> Result<S3InventoryScan> {
    let source = S3Inventory {
        bucket: bucket.to_string(),
        selector: InventorySelector::Prefix(key.to_string()),
    };
    let mut scan = list_s3_inventory_with_config(&source, config).await?;
    scan.objects.retain(|object| object.key == key);
    Ok(scan)
}

pub async fn ensure_bucket_versioning_enabled(config: &S3ClientConfig, bucket: &str) -> Result<()> {
    let client = config.client_for_bucket(bucket).await?;
    let output = client
        .get_bucket_versioning()
        .bucket(bucket)
        .send()
        .await
        .map_err(|error| {
            input_error(
                &format!("s3://{bucket}"),
                &format!(
                    "GetBucketVersioning failed: {} (requires s3:GetBucketVersioning permission)",
                    safe_sdk_error(&error)
                ),
            )
        })?;
    versioning_status_result(bucket, output.status())
}

pub(crate) fn versioning_status_result(
    bucket: &str,
    status: Option<&BucketVersioningStatus>,
) -> Result<()> {
    let detail = match status {
        Some(BucketVersioningStatus::Enabled) => return Ok(()),
        Some(BucketVersioningStatus::Suspended) => {
            "bucket versioning status is Suspended".to_string()
        }
        Some(other) if !other.as_str().is_empty() => {
            format!("bucket versioning status is {}", other.as_str())
        }
        _ => "versioning has never been enabled on this bucket".to_string(),
    };
    Err(input_error(
        &format!("s3://{bucket}"),
        &format!("--compact requires S3 bucket versioning to be Enabled; {detail}"),
    ))
}

pub async fn load_s3_version(
    inventory: &S3Inventory,
    key: &str,
    version_id: &str,
) -> Result<LoadedInput> {
    load_s3_version_cached(inventory, key, version_id, None, &S3CachePolicy::default()).await
}

#[derive(Clone)]
pub(crate) struct S3VersionReader {
    config: S3ClientConfig,
    policy: S3CachePolicy,
}

impl S3VersionReader {
    pub(crate) async fn new(policy: &S3CachePolicy) -> Result<Self> {
        Self::new_with_config(policy, &S3ClientConfig::new(None)).await
    }

    pub(crate) async fn new_with_config(
        policy: &S3CachePolicy,
        config: &S3ClientConfig,
    ) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            policy: policy.clone(),
        })
    }

    async fn load_version(
        &self,
        inventory: &S3Inventory,
        key: &str,
        version_id: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<LoadedInput> {
        let client = self.config.client_for_bucket(&inventory.bucket).await?;
        load_s3_version_with(
            &client,
            key,
            &inventory.bucket,
            version_id,
            etag,
            last_modified,
            &self.policy,
        )
        .await
    }

    pub(crate) async fn load_selected(
        &self,
        inventory: &S3Inventory,
        key: &str,
        version: &S3InventoryVersion,
    ) -> Result<LoadedInput> {
        let Some(version_id) = version.version_id.as_deref() else {
            return Err(input_error(
                &format!("s3://{}/{}", inventory.bucket, key),
                "S3 version has no version id",
            ));
        };
        self.load_version(
            inventory,
            key,
            version_id,
            version.etag.as_deref(),
            version.last_modified.as_deref(),
        )
        .await
    }

    async fn load_pair(
        &self,
        inventory: &S3Inventory,
        job: &ComparisonJob,
    ) -> Result<(LoadedInput, LoadedInput)> {
        let (left, right) = tokio::try_join!(
            self.load_version(
                inventory,
                &job.event.key,
                &job.left_id,
                job.left_etag.as_deref(),
                job.left_modified.as_deref(),
            ),
            self.load_version(
                inventory,
                &job.event.key,
                &job.version_id,
                job.right_etag.as_deref(),
                job.right_modified.as_deref(),
            ),
        )?;
        Ok((left, right))
    }

    pub(crate) async fn compare_pair(
        &self,
        inventory: &S3Inventory,
        job: ComparisonJob,
    ) -> Result<Option<crate::TerraformPlan>> {
        let (left, right) = self.load_pair(inventory, &job).await?;
        match (
            classify_state_content(&left.contents),
            classify_state_content(&right.contents),
        ) {
            (StateContent::TerraformV4, StateContent::TerraformV4) => {
                let mut plan = crate::compare_states(&left.contents, &right.contents)?.plan;
                if left.version.is_some() || right.version.is_some() {
                    plan.source_versions = Some(crate::SourceVersions {
                        left_label: "Previous Version".to_string(),
                        right_label: "Selected Version".to_string(),
                        left: left.version,
                        right: right.version,
                    });
                }
                Ok(Some(plan))
            }
            (StateContent::Unrelated, StateContent::Unrelated) => Ok(None),
            (left, right) => {
                let reason = match (&left, &right) {
                    (StateContent::Invalid(message), _) | (_, StateContent::Invalid(message)) => {
                        message.clone()
                    }
                    _ => "S3 history entries contain different Terraform formats".into(),
                };
                Err(TfdiffError::ParseError(reason))
            }
        }
    }
}

async fn load_s3_version_with(
    client: &Client,
    key: &str,
    bucket: &str,
    version_id: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    policy: &S3CachePolicy,
) -> Result<LoadedInput> {
    let object = S3Object {
        bucket: bucket.to_string(),
        key: key.to_string(),
        selector: VersionSelector::Version(version_id.to_string()),
    };
    let history_metadata = if etag.is_none() || last_modified.is_none() {
        list_history_for_version(client, &object, version_id)
            .await?
            .into_iter()
            .find(|entry| entry.history_id() == Some(version_id))
            .and_then(|entry| entry.metadata())
    } else {
        None
    };
    let etag = etag.or_else(|| history_metadata.as_ref()?.etag.as_deref());
    let last_modified =
        last_modified.or_else(|| history_metadata.as_ref()?.last_modified.as_deref());
    let (contents, mut source_version) = get_version(
        client,
        &object,
        Some(version_id),
        etag,
        last_modified,
        policy,
    )
    .await?;
    source_version.version_id = Some(version_id.to_string());
    if source_version.etag.is_none() {
        source_version.etag = history_metadata.and_then(|metadata| metadata.etag);
    }
    Ok(LoadedInput {
        contents,
        version: Some(source_version),
    })
}

pub async fn load_s3_version_cached(
    inventory: &S3Inventory,
    key: &str,
    version_id: &str,
    etag: Option<&str>,
    policy: &S3CachePolicy,
) -> Result<LoadedInput> {
    let reader = S3VersionReader::new(policy).await?;
    reader
        .load_version(inventory, key, version_id, etag, None)
        .await
}

async fn list_s3_inventory_uncached(
    source: &S3Inventory,
    client: &Client,
) -> Result<S3InventoryScan> {
    let (prefix, matcher) = match &source.selector {
        InventorySelector::Prefix(prefix) => (prefix.clone(), None),
        InventorySelector::Regex { prefix, pattern } => {
            let regex = crate::pattern::anchored_regex(pattern).map_err(|error| {
                input_error(
                    &format!("s3://{}/{}", source.bucket, pattern),
                    &format!("invalid S3 regex pattern: {error}"),
                )
            })?;
            (prefix.clone(), Some(KeyMatcher::Regex(regex)))
        }
        InventorySelector::Glob { prefix, pattern } => {
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| {
                    input_error(
                        &format!("s3://{}/{}", source.bucket, pattern),
                        &format!("invalid S3 glob pattern: {error}"),
                    )
                })?
                .compile_matcher();
            (prefix.clone(), Some(KeyMatcher::Glob(matcher)))
        }
    };
    let uri = format!("s3://{}/{}", source.bucket, prefix);
    let mut keys: HashMap<String, Vec<S3InventoryVersion>> = HashMap::new();
    let mut key_marker: Option<String> = None;
    let mut version_id_marker: Option<String> = None;
    let mut seen_markers = HashSet::new();
    let spinner = listing_spinner(&uri);
    let mut pages_listed: u64 = 0;
    loop {
        pages_listed += 1;
        spinner.set_message(format!("{uri}: listing {pages_listed} page(s)"));
        let mut request = client
            .list_object_versions()
            .bucket(&source.bucket)
            .prefix(&prefix)
            .max_keys(1000)
            .encoding_type(EncodingType::Url);
        if let Some(marker) = &key_marker {
            request = request.key_marker(marker);
        }
        if let Some(marker) = &version_id_marker {
            request = request.version_id_marker(marker);
        }
        let page = match request.send().await {
            Ok(page) => page,
            Err(error) => {
                // Without credentials nothing can be listed; that is no partial result.
                if let Some(message) = sdk_credentials_failure(&error) {
                    return Err(input_error(&uri, &message));
                }
                let message = format!("ListObjectVersions failed: {}", safe_sdk_error(&error));
                return Ok(S3InventoryScan {
                    objects: into_inventory_objects(keys),
                    errors: vec![format!("{uri}: {message}")],
                });
            }
        };
        // ListObjectVersions returns separate arrays; retain IsLatest to resolve timestamp ties.
        for version in page.versions() {
            let Some(raw_key) = version.key() else {
                continue;
            };
            let key = decode_listed_key(raw_key, page.encoding_type());
            if !key_matches(&key, &prefix, matcher.as_ref()) {
                continue;
            }
            let last_modified = version
                .last_modified()
                .and_then(|timestamp| timestamp.fmt(Format::DateTimeWithOffset).ok());
            keys.entry(key).or_default().push(S3InventoryVersion {
                version_id: version.version_id().map(str::to_string),
                last_modified,
                is_delete_marker: false,
                is_latest: version.is_latest() == Some(true),
                size: version.size(),
                etag: version.e_tag().map(str::to_string),
            });
        }
        for marker in page.delete_markers() {
            let Some(raw_key) = marker.key() else {
                continue;
            };
            let key = decode_listed_key(raw_key, page.encoding_type());
            if !key_matches(&key, &prefix, matcher.as_ref()) {
                continue;
            }
            let last_modified = marker
                .last_modified()
                .and_then(|timestamp| timestamp.fmt(Format::DateTimeWithOffset).ok());
            keys.entry(key).or_default().push(S3InventoryVersion {
                version_id: marker.version_id().map(str::to_string),
                last_modified,
                is_delete_marker: true,
                is_latest: marker.is_latest() == Some(true),
                size: None,
                etag: None,
            });
        }
        if page.is_truncated() != Some(true) {
            break;
        }
        let Some(next_key_marker) = page.next_key_marker() else {
            return Ok(S3InventoryScan {
                objects: into_inventory_objects(keys),
                errors: vec![format!(
                    "{uri}: S3 returned a truncated inventory page without a key marker"
                )],
            });
        };
        let Some(next_version_id_marker) = page.next_version_id_marker() else {
            return Ok(S3InventoryScan {
                objects: into_inventory_objects(keys),
                errors: vec![format!(
                    "{uri}: S3 returned a truncated inventory page without a version marker"
                )],
            });
        };
        let next_markers = (
            decode_listed_key(next_key_marker, page.encoding_type()),
            next_version_id_marker.to_string(),
        );
        if next_markers.0 == key_marker.as_deref().unwrap_or_default()
            && next_markers.1 == version_id_marker.as_deref().unwrap_or_default()
            || !seen_markers.insert(next_markers.clone())
        {
            return Ok(S3InventoryScan {
                objects: into_inventory_objects(keys),
                errors: vec![format!(
                    "{uri}: S3 returned non-progressing inventory continuation markers"
                )],
            });
        }
        key_marker = Some(next_markers.0);
        version_id_marker = Some(next_markers.1);
    }

    Ok(S3InventoryScan {
        objects: into_inventory_objects(keys),
        errors: Vec::new(),
    })
}

/// Spinner shown on stderr while an inventory listing pages through S3. It is hidden when
/// stderr is not a terminal and is cleared on drop, so every return path is covered.
struct ListingSpinner(indicatif::ProgressBar);

impl ListingSpinner {
    fn set_message(&self, message: String) {
        self.0.set_message(message);
    }
}

impl Drop for ListingSpinner {
    fn drop(&mut self) {
        self.0.finish_and_clear();
    }
}

fn listing_spinner(uri: &str) -> ListingSpinner {
    use std::io::IsTerminal;
    let bar = if std::io::stderr().is_terminal() {
        let bar =
            indicatif::ProgressBar::with_draw_target(None, indicatif::ProgressDrawTarget::stderr());
        bar.set_style(
            indicatif::ProgressStyle::with_template("tfdiff: {spinner} {msg}")
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
        );
        bar.set_message(format!("{uri}: listing"));
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar
    } else {
        indicatif::ProgressBar::hidden()
    };
    ListingSpinner(bar)
}

fn into_inventory_objects(
    keys: HashMap<String, Vec<S3InventoryVersion>>,
) -> Vec<S3InventoryObject> {
    let mut inventory: Vec<_> = keys
        .into_iter()
        .map(|(key, mut history)| {
            history.sort_by(compare_inventory_versions);
            S3InventoryObject { key, history }
        })
        .collect();
    inventory.sort_by(|left, right| left.key.cmp(&right.key));
    inventory
}

pub(crate) fn compare_inventory_versions(
    left: &S3InventoryVersion,
    right: &S3InventoryVersion,
) -> std::cmp::Ordering {
    compare_timestamps_desc(
        left.last_modified.as_deref(),
        right.last_modified.as_deref(),
    )
    .then_with(|| right.is_latest.cmp(&left.is_latest))
}

enum KeyMatcher {
    Glob(GlobMatcher),
    Regex(regex::Regex),
}

fn key_matches(key: &str, prefix: &str, matcher: Option<&KeyMatcher>) -> bool {
    if let Some(matcher) = matcher {
        match matcher {
            KeyMatcher::Glob(glob) => glob.is_match(key),
            KeyMatcher::Regex(regex) => regex.is_match(key),
        }
    } else if prefix.is_empty() {
        true
    } else {
        key.starts_with(prefix)
    }
}

fn compare_timestamps_desc(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    let left = left.and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
    let right = right.and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
    match (left, right) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub enum S3InputResolution {
    VersionDiff(LoadedInput, LoadedInput),
    Inventory(Vec<S3InventoryObject>),
}

pub async fn resolve_s3_input(source: &S3Object) -> Result<S3InputResolution> {
    resolve_s3_input_cached(source, &S3CachePolicy::default()).await
}

pub async fn resolve_s3_input_cached(
    source: &S3Object,
    policy: &S3CachePolicy,
) -> Result<S3InputResolution> {
    resolve_s3_input_cached_with_config(source, policy, &S3ClientConfig::new(None)).await
}

pub async fn resolve_s3_input_cached_with_config(
    source: &S3Object,
    policy: &S3CachePolicy,
    config: &S3ClientConfig,
) -> Result<S3InputResolution> {
    let client = config.client_for_bucket(&source.bucket).await?;
    if !matches!(source.selector, VersionSelector::Current) {
        let pair =
            read_version_diff_with(&InputSource::S3(source.clone()), &client, policy).await?;
        return Ok(S3InputResolution::VersionDiff(pair.0, pair.1));
    }
    let history = list_history(&client, source, usize::MAX, None).await?;
    if !history.is_empty() {
        let pair = read_version_diff_from_history(source, &client, history, policy).await?;
        return Ok(S3InputResolution::VersionDiff(pair.0, pair.1));
    }
    let prefix_source = S3Inventory {
        bucket: source.bucket.clone(),
        selector: InventorySelector::Prefix(format!("{}/", source.key)),
    };
    let inventory = list_s3_inventory_uncached(&prefix_source, &client).await?;
    if let Some(error) = inventory.errors.first() {
        return Err(input_error(
            &format!("s3://{}/{}", source.bucket, source.key),
            error,
        ));
    }
    Ok(S3InputResolution::Inventory(inventory.objects))
}

pub async fn read_version_diff(source: &InputSource) -> Result<(LoadedInput, LoadedInput)> {
    read_version_diff_cached(source, &S3CachePolicy::default()).await
}

pub async fn read_version_diff_cached(
    source: &InputSource,
    policy: &S3CachePolicy,
) -> Result<(LoadedInput, LoadedInput)> {
    read_version_diff_cached_with_config(source, policy, &S3ClientConfig::new(None)).await
}

pub async fn read_version_diff_cached_with_config(
    source: &InputSource,
    policy: &S3CachePolicy,
    config: &S3ClientConfig,
) -> Result<(LoadedInput, LoadedInput)> {
    let bucket = match source {
        InputSource::S3(object) => object.bucket.as_str(),
        _ => {
            return Err(input_error(
                &source.description(),
                "implicit version comparisons require an S3 object",
            ));
        }
    };
    let client = config.client_for_bucket(bucket).await?;
    read_version_diff_with(source, &client, policy).await
}

pub async fn read_version_diff_at_bounds(
    object: &S3Object,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(LoadedInput, LoadedInput)> {
    read_version_diff_at_bounds_cached(object, since, until, now, &S3CachePolicy::default()).await
}

pub async fn read_version_diff_at_bounds_cached(
    object: &S3Object,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    policy: &S3CachePolicy,
) -> Result<(LoadedInput, LoadedInput)> {
    read_version_diff_at_bounds_cached_with_config(
        object,
        since,
        until,
        now,
        policy,
        &S3ClientConfig::new(None),
    )
    .await
}

pub async fn read_version_diff_at_bounds_cached_with_config(
    object: &S3Object,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    policy: &S3CachePolicy,
    config: &S3ClientConfig,
) -> Result<(LoadedInput, LoadedInput)> {
    let client = config.client_for_bucket(&object.bucket).await?;
    read_version_diff_at_bounds_with(object, since, until, now, &client, policy).await
}

async fn read_version_diff_at_bounds_with(
    object: &S3Object,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    client: &Client,
    policy: &S3CachePolicy,
) -> Result<(LoadedInput, LoadedInput)> {
    let object = S3Object {
        selector: VersionSelector::Current,
        ..object.clone()
    };
    let history = list_history(client, &object, usize::MAX, None).await?;
    let since_bound = since;
    let until_bound = until.or_else(|| since_bound.map(|_| now));
    let error_source = format!("s3://{}/{}", object.bucket, object.key);
    let select_at_or_before = |bound: chrono::DateTime<chrono::Utc>, label: &str| {
        history
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .last_modified()
                    .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
                    .filter(|timestamp| timestamp.with_timezone(&chrono::Utc) <= bound)
                    .map(|timestamp| (index, timestamp))
            })
            .max_by(|(left_index, left_time), (right_index, right_time)| {
                left_time
                    .cmp(right_time)
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(index, _)| index)
            .ok_or_else(|| {
                input_error(
                    &error_source,
                    &format!("no S3 history entry exists at or before {label}"),
                )
            })
    };
    let right_bound = until_bound.ok_or_else(|| {
        input_error(
            &error_source,
            "an --until or --since time bound is required",
        )
    })?;
    if since_bound.is_some_and(|bound| bound > right_bound) {
        return Err(input_error(
            &error_source,
            "--since must be earlier than or equal to --until",
        ));
    }
    let right_offset = select_at_or_before(right_bound, "the right-side time bound")?;
    let left_offset = if let Some(since_bound) = since_bound {
        select_at_or_before(since_bound, "the --since time bound")?
    } else {
        right_offset.checked_add(1).ok_or_else(|| {
            input_error(
                &error_source,
                "selected S3 version has no immediately preceding retained history entry",
            )
        })?
    };
    if left_offset == right_offset {
        return Err(input_error(
            &error_source,
            "time bounds select the same S3 history entry; choose distinct bounds",
        ));
    }
    let left = history.get(left_offset).ok_or_else(|| {
        input_error(
            &error_source,
            "no immediately preceding S3 history entry exists at the selected time",
        )
    })?;
    let right = history.get(right_offset).ok_or_else(|| {
        input_error(
            &error_source,
            "no S3 history entry exists at the right-side time bound",
        )
    })?;
    for (entry, side) in [(left, "left"), (right, "right")] {
        if matches!(entry, HistoryEntry::DeleteMarker { .. }) {
            return Err(input_error(
                &error_source,
                &format!("selected {side}-side S3 history entry is a delete marker"),
            ));
        }
    }
    read_history_pair(&object, client, left.clone(), right.clone(), policy).await
}

async fn read_version_diff_with(
    source: &InputSource,
    client: &Client,
    policy: &S3CachePolicy,
) -> Result<(LoadedInput, LoadedInput)> {
    let InputSource::S3(object) = source else {
        return Err(input_error(
            &source.description(),
            "implicit version comparisons require an S3 object",
        ));
    };
    let history = list_history_for_object(client, object).await?;
    read_version_diff_from_history(object, client, history, policy).await
}

async fn read_version_diff_from_history(
    object: &S3Object,
    client: &Client,
    history: Vec<HistoryEntry>,
    policy: &S3CachePolicy,
) -> Result<(LoadedInput, LoadedInput)> {
    let right_offset = match &object.selector {
        VersionSelector::Current => 0,
        VersionSelector::HeadOffset(offset) => *offset,
        VersionSelector::Version(version_id) => history
            .iter()
            .position(|entry| entry.history_id() == Some(version_id.as_str()))
            .ok_or_else(|| {
                input_error(
                    &format!("s3://{}/{}", object.bucket, object.key),
                    "requested S3 version was not found in object history",
                )
            })?,
    };
    let right = history.get(right_offset).ok_or_else(|| {
        input_error(
            &format!("s3://{}/{}", object.bucket, object.key),
            "requested S3 version was not found in object history",
        )
    })?;
    if matches!(right, HistoryEntry::DeleteMarker { .. }) {
        let selected = match &object.selector {
            VersionSelector::HeadOffset(offset) => format!("HEAD~{offset}"),
            VersionSelector::Version(version_id) => format!("version {version_id}"),
            VersionSelector::Current => "HEAD".to_string(),
        };
        return Err(input_error(
            &format!("s3://{}/{}", object.bucket, object.key),
            &format!("selected {selected} is a delete marker"),
        ));
    }
    let predecessor_offset = right_offset.checked_add(1).ok_or_else(|| {
        input_error(
            &format!("s3://{}/{}", object.bucket, object.key),
            "selected S3 version has no immediately preceding retained version",
        )
    })?;
    let left = history.get(predecessor_offset).ok_or_else(|| {
        input_error(
            &format!("s3://{}/{}", object.bucket, object.key),
            "selected S3 version has no immediately preceding retained version",
        )
    })?;
    if matches!(left, HistoryEntry::DeleteMarker { .. }) {
        return Err(input_error(
            &format!("s3://{}/{}", object.bucket, object.key),
            "immediately preceding S3 history entry is a delete marker",
        ));
    }
    read_history_pair(object, client, left.clone(), right.clone(), policy).await
}

async fn read_history_pair(
    object: &S3Object,
    client: &Client,
    left: HistoryEntry,
    right: HistoryEntry,
    policy: &S3CachePolicy,
) -> Result<(LoadedInput, LoadedInput)> {
    let read = |entry: HistoryEntry| async move {
        let version_id = entry.version_id().expect("validated object version");
        let etag = entry.etag();
        let last_modified = entry.last_modified();
        let (contents, fetched_metadata) = get_version(
            client,
            object,
            Some(version_id),
            etag,
            last_modified,
            policy,
        )
        .await?;
        let mut metadata = entry.metadata().unwrap_or(fetched_metadata.clone());
        if metadata.last_modified.is_none() {
            metadata.last_modified = fetched_metadata.last_modified;
        }
        if metadata.etag.is_none() {
            metadata.etag = fetched_metadata.etag;
        }
        Ok::<_, TfdiffError>(LoadedInput {
            contents,
            version: Some(metadata),
        })
    };
    let (left, right) = tokio::try_join!(read(left), read(right))?;
    Ok((left, right))
}

pub async fn load_inputs(sources: &[InputSource]) -> Result<Vec<LoadedInput>> {
    load_inputs_cached(sources, &S3CachePolicy::default()).await
}

pub async fn load_inputs_cached(
    sources: &[InputSource],
    policy: &S3CachePolicy,
) -> Result<Vec<LoadedInput>> {
    load_inputs_cached_with_config(sources, policy, &S3ClientConfig::new(None)).await
}

pub async fn load_inputs_cached_with_config(
    sources: &[InputSource],
    policy: &S3CachePolicy,
    config: &S3ClientConfig,
) -> Result<Vec<LoadedInput>> {
    let s3_input_count = sources
        .iter()
        .filter(|source| matches!(source, InputSource::S3(_)))
        .count();
    if s3_input_count > 1 {
        let _ = writeln!(
            std::io::stderr(),
            "tfdiff: reading {s3_input_count} S3 object inputs (cache hits may avoid downloads)..."
        );
    }
    let needs_client = sources
        .iter()
        .any(|source| matches!(source, InputSource::S3(_)));

    let mut completed_s3_inputs = 0;
    let mut history_offsets: HashMap<(String, String), usize> = HashMap::new();
    for source in sources {
        let InputSource::S3(object) = source else {
            continue;
        };
        if let VersionSelector::HeadOffset(offset) = object.selector {
            history_offsets
                .entry((object.bucket.clone(), object.key.clone()))
                .and_modify(|max_offset| *max_offset = (*max_offset).max(offset))
                .or_insert(offset);
        }
    }

    let mut clients = HashMap::new();
    if needs_client {
        for source in sources {
            if let InputSource::S3(object) = source {
                if !clients.contains_key(&object.bucket) {
                    clients.insert(
                        object.bucket.clone(),
                        config.client_for_bucket(&object.bucket).await?,
                    );
                }
            }
        }
    }
    let mut resolved_versions = HashMap::new();
    for source in sources {
        let InputSource::S3(object) = source else {
            continue;
        };
        let key = (object.bucket.clone(), object.key.clone());
        if history_offsets.remove(&key).is_some() {
            let history = list_history_for_object(
                clients
                    .get(&object.bucket)
                    .expect("S3 client initialized for S3 input"),
                object,
            )
            .await?;
            resolved_versions.insert(key, history);
        }
    }

    let mut contents = Vec::with_capacity(sources.len());
    for source in sources {
        match source {
            InputSource::Local(path) => contents.push(LoadedInput {
                contents: std::fs::read_to_string(path).map_err(TfdiffError::IoError)?,
                version: None,
            }),
            InputSource::S3Inventory(_) => {
                return Err(input_error(
                    &source.description(),
                    "S3 inventory inputs must be expanded before reading contents",
                ));
            }
            InputSource::S3(object) => {
                completed_s3_inputs += 1;
                if s3_input_count > 1 {
                    let _ = writeln!(
                        std::io::stderr(),
                        "tfdiff: reading S3 object {completed_s3_inputs}/{s3_input_count}: s3://{}/{}",
                        object.bucket,
                        object.key
                    );
                }
                let key = (object.bucket.clone(), object.key.clone());
                let selected = match &object.selector {
                    VersionSelector::Current => None,
                    VersionSelector::Version(version_id) => {
                        let history = list_history_for_version(
                            clients
                                .get(&object.bucket)
                                .expect("S3 client initialized for S3 input"),
                            object,
                            version_id,
                        )
                        .await?;
                        history
                            .into_iter()
                            .find(|entry| entry.history_id() == Some(version_id.as_str()))
                    }
                    VersionSelector::HeadOffset(offset) => {
                        let entries = &resolved_versions[&key];
                        let Some(entry) = entries.get(*offset) else {
                            return Err(input_error(
                                &format!("s3://{}/{}", object.bucket, object.key),
                                &format!(
                                    "history entry HEAD~{offset} does not exist; the key has only {} retained entries",
                                    entries.len()
                                ),
                            ));
                        };
                        Some(entry.clone())
                    }
                };
                if matches!(selected, Some(HistoryEntry::DeleteMarker { .. })) {
                    let offset = match object.selector {
                        VersionSelector::HeadOffset(offset) => offset,
                        _ => {
                            unreachable!("explicit version selectors cannot resolve delete markers")
                        }
                    };
                    return Err(input_error(
                        &format!("s3://{}/{}", object.bucket, object.key),
                        &format!("history entry HEAD~{offset} is a delete marker"),
                    ));
                }
                let selected_version = selected.as_ref().and_then(HistoryEntry::version_id);
                let metadata = selected.as_ref().and_then(HistoryEntry::metadata);
                let (result, fetched_metadata) = get_version(
                    clients
                        .get(&object.bucket)
                        .expect("S3 client initialized for S3 input"),
                    object,
                    selected_version,
                    selected.as_ref().and_then(HistoryEntry::etag),
                    selected.as_ref().and_then(HistoryEntry::last_modified),
                    policy,
                )
                .await?;
                let fetched_metadata = if selected_version.is_none() {
                    recover_source_version_metadata(
                        clients
                            .get(&object.bucket)
                            .expect("S3 client initialized for S3 input"),
                        object,
                        fetched_metadata,
                    )
                    .await
                } else {
                    fetched_metadata
                };
                let version = metadata
                    .or_else(|| {
                        (fetched_metadata.version_id.is_some()
                            || fetched_metadata.last_modified.is_some()
                            || fetched_metadata.etag.is_some())
                        .then_some(fetched_metadata.clone())
                    })
                    .map(|mut metadata| {
                        if metadata.version_id.is_none() {
                            metadata.version_id = fetched_metadata.version_id.clone();
                        }
                        if metadata.last_modified.is_none() {
                            metadata.last_modified = fetched_metadata.last_modified.clone();
                        }
                        if metadata.etag.is_none() {
                            metadata.etag = fetched_metadata.etag.clone();
                        }
                        metadata
                    });
                contents.push(LoadedInput {
                    contents: result,
                    version,
                });
            }
        }
    }
    Ok(contents)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum HistoryEntry {
    Version {
        version_id: String,
        last_modified: Option<String>,
        etag: Option<String>,
    },
    DeleteMarker {
        version_id: Option<String>,
        last_modified: Option<String>,
    },
}

impl HistoryEntry {
    fn last_modified(&self) -> Option<&str> {
        match self {
            Self::Version { last_modified, .. } | Self::DeleteMarker { last_modified, .. } => {
                last_modified.as_deref()
            }
        }
    }

    fn etag(&self) -> Option<&str> {
        match self {
            Self::Version { etag, .. } => etag.as_deref(),
            Self::DeleteMarker { .. } => None,
        }
    }

    fn version_id(&self) -> Option<&str> {
        match self {
            Self::Version { version_id, .. } => Some(version_id),
            Self::DeleteMarker { .. } => None,
        }
    }

    fn history_id(&self) -> Option<&str> {
        match self {
            Self::Version { version_id, .. } => Some(version_id),
            Self::DeleteMarker { version_id, .. } => version_id.as_deref(),
        }
    }

    fn metadata(&self) -> Option<crate::SourceVersion> {
        match self {
            Self::Version {
                version_id,
                last_modified,
                etag,
            } => Some(crate::SourceVersion {
                version_id: Some(version_id.clone()),
                last_modified: last_modified.clone(),
                etag: etag.clone(),
            }),
            Self::DeleteMarker { .. } => None,
        }
    }
}

async fn list_history_for_version(
    client: &Client,
    object: &S3Object,
    target_version: &str,
) -> Result<Vec<HistoryEntry>> {
    let mut object_history = object.clone();
    object_history.selector = VersionSelector::Current;
    let history = list_history(client, &object_history, 0, Some(target_version)).await?;
    if history
        .iter()
        .any(|entry| entry.history_id() == Some(target_version))
    {
        return Ok(history);
    }
    Err(input_error(
        &format!("s3://{}/{}", object.bucket, object.key),
        "requested S3 version was not found in object history",
    ))
}

async fn list_history_for_object(client: &Client, object: &S3Object) -> Result<Vec<HistoryEntry>> {
    let (max_offset, target_version) = match &object.selector {
        VersionSelector::Current => (1, None),
        VersionSelector::HeadOffset(offset) => (
            offset.checked_add(1).ok_or_else(|| {
                input_error(
                    &format!("s3://{}/{}", object.bucket, object.key),
                    "selected S3 version has no representable preceding history offset",
                )
            })?,
            None,
        ),
        VersionSelector::Version(version_id) => (0, Some(version_id.as_str())),
    };
    if let Some(target_version) = target_version {
        list_history_for_version(client, object, target_version).await
    } else {
        list_history(client, object, max_offset, None).await
    }
}

async fn list_history(
    client: &Client,
    object: &S3Object,
    max_offset: usize,
    target_version: Option<&str>,
) -> Result<Vec<HistoryEntry>> {
    let mut history = Vec::new();
    let mut target_index = None;
    let mut key_marker: Option<String> = None;
    let mut version_id_marker: Option<String> = None;
    let mut seen_markers = HashSet::new();
    loop {
        let mut request = client
            .list_object_versions()
            .bucket(&object.bucket)
            .prefix(&object.key)
            .max_keys(1)
            .encoding_type(EncodingType::Url);
        if let Some(marker) = &key_marker {
            request = request.key_marker(marker);
        }
        if let Some(marker) = &version_id_marker {
            request = request.version_id_marker(marker);
        }
        let page = request.send().await.map_err(|error| {
            input_error(
                &format!("s3://{}/{}", object.bucket, object.key),
                &format!("ListObjectVersions failed: {}", safe_sdk_error(&error)),
            )
        })?;
        let page_entries = history_entries(&page, &object.key)?;
        let page_start = history.len();
        if let Some(target) = target_version {
            target_index = target_index.or_else(|| {
                page_entries
                    .iter()
                    .position(|entry| entry.history_id() == Some(target))
                    .map(|position| page_start + position)
            });
        }
        history.extend(page_entries);
        let target_has_predecessor =
            target_index.is_some_and(|position| history.len() > position + 1);
        if (target_has_predecessor || (target_version.is_none() && history.len() > max_offset))
            && !(max_offset == usize::MAX && target_version.is_none())
        {
            break;
        }
        if page.is_truncated() != Some(true) {
            break;
        }
        let next_key_marker = page.next_key_marker().ok_or_else(|| {
            input_error(
                &format!("s3://{}/{}", object.bucket, object.key),
                "S3 returned a truncated history page without a key continuation marker",
            )
        })?;
        let next_version_id_marker = page.next_version_id_marker().ok_or_else(|| {
            input_error(
                &format!("s3://{}/{}", object.bucket, object.key),
                "S3 returned a truncated history page without a version continuation marker",
            )
        })?;
        let next_markers = (
            decode_listed_key(next_key_marker, page.encoding_type()),
            next_version_id_marker.to_string(),
        );
        if next_markers.0 == key_marker.as_deref().unwrap_or_default()
            && next_markers.1 == version_id_marker.as_deref().unwrap_or_default()
            || !seen_markers.insert(next_markers.clone())
        {
            return Err(input_error(
                &format!("s3://{}/{}", object.bucket, object.key),
                "S3 returned non-progressing history continuation markers",
            ));
        }
        key_marker = Some(next_markers.0);
        version_id_marker = Some(next_markers.1);
    }
    Ok(history)
}

fn history_entries(page: &ListObjectVersionsOutput, key: &str) -> Result<Vec<HistoryEntry>> {
    // Preserve S3's version order for timestamp ties using IsLatest as the stable anchor.
    let mut entries: Vec<_> = page
        .versions()
        .iter()
        .filter_map(|version| {
            version.key().map(|entry_key| {
                (
                    decode_listed_key(entry_key, page.encoding_type()),
                    version.version_id(),
                    version.last_modified(),
                    version.is_latest() == Some(true),
                    version.e_tag(),
                    false,
                )
            })
        })
        .chain(page.delete_markers().iter().filter_map(|marker| {
            marker.key().map(|entry_key| {
                (
                    decode_listed_key(entry_key, page.encoding_type()),
                    marker.version_id(),
                    marker.last_modified(),
                    marker.is_latest() == Some(true),
                    None,
                    true,
                )
            })
        }))
        .filter(|(entry_key, _, _, _, _, _)| entry_key == key)
        .map(
            |(entry_key, version_id, last_modified, is_latest, etag, delete_marker)| {
                let last_modified = last_modified
                    .and_then(|timestamp| timestamp.fmt(Format::DateTimeWithOffset).ok());
                (
                    entry_key,
                    version_id,
                    last_modified,
                    is_latest,
                    etag,
                    delete_marker,
                )
            },
        )
        .collect();

    entries.sort_by(|left, right| match (&left.2, &right.2) {
        (Some(left_time), Some(right_time)) => {
            right_time.cmp(left_time).then_with(|| right.3.cmp(&left.3))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right.3.cmp(&left.3),
    });

    entries
        .into_iter()
        .map(|(_, version_id, last_modified, _, etag, delete_marker)| {
            if delete_marker {
                Ok(HistoryEntry::DeleteMarker {
                    version_id: version_id.map(str::to_string),
                    last_modified,
                })
            } else {
                version_id
                    .map(|version_id| HistoryEntry::Version {
                        version_id: version_id.to_string(),
                        last_modified,
                        etag: etag.map(str::to_string),
                    })
                    .ok_or_else(|| {
                        TfdiffError::ParseError(
                            "S3 returned an object version without a version ID".into(),
                        )
                    })
            }
        })
        .collect()
}

fn decode_listed_key(key: &str, encoding_type: Option<&EncodingType>) -> String {
    if encoding_type.is_some_and(|encoding| encoding.as_str() == "url") {
        percent_decode(key).unwrap_or_else(|_| key.to_string())
    } else {
        key.to_string()
    }
}

async fn recover_source_version_metadata(
    client: &Client,
    object: &S3Object,
    mut source_version: crate::SourceVersion,
) -> crate::SourceVersion {
    if source_version.version_id.is_some()
        && source_version.last_modified.is_some()
        && source_version.etag.is_some()
    {
        return source_version;
    }

    let history = if let Some(version_id) = source_version.version_id.as_deref() {
        list_history_for_version(client, object, version_id).await
    } else {
        list_history_for_object(client, object).await
    };
    if let Ok(history) = history {
        if let Some(metadata) = history
            .iter()
            .find(|entry| {
                source_version
                    .version_id
                    .as_deref()
                    .is_none_or(|version_id| entry.history_id() == Some(version_id))
            })
            .and_then(HistoryEntry::metadata)
        {
            source_version.version_id = source_version.version_id.or(metadata.version_id);
            source_version.last_modified = source_version.last_modified.or(metadata.last_modified);
            source_version.etag = source_version.etag.or(metadata.etag);
        }
    }
    source_version
}

async fn get_version(
    client: &Client,
    object: &S3Object,
    version_id: Option<&str>,
    etag: Option<&str>,
    last_modified: Option<&str>,
    policy: &S3CachePolicy,
) -> Result<(String, crate::SourceVersion)> {
    let etag = if version_id.is_none() && etag.is_none() {
        policy.cache().and_then(|cache| {
            let pointer_key = object_cache_key(client, object, None, None);
            cache
                .load::<String>("current-object-etags", &pointer_key)
                .map(|entry| entry.payload)
        })
    } else {
        etag.map(str::to_string)
    };
    let cache_key = object_cache_key(client, object, version_id, etag.as_deref());
    if let Some(cache) = policy.cache() {
        let cached = if let Some(etag) = etag.as_deref() {
            cache.load_body(&cache_key, etag)
        } else if version_id.is_some() {
            cache
                .load::<crate::cache::CachedBody>("object-bodies", &cache_key)
                .map(|entry| entry.payload)
                .filter(|body| {
                    body.length == body.bytes.len() as u64
                        && crate::cache::sha256_hex(&body.bytes) == body.sha256
                })
        } else {
            None
        };
        if let Some(body) = cached {
            if version_id.is_some() {
                let mut metadata = body.source_version.unwrap_or(crate::SourceVersion {
                    version_id: version_id.map(str::to_string),
                    last_modified: None,
                    etag: None,
                });
                if metadata.version_id.is_none() {
                    metadata.version_id = version_id.map(str::to_string);
                }
                if metadata.last_modified.is_none() {
                    metadata.last_modified = last_modified.map(str::to_string);
                }
                if metadata.etag.is_none() {
                    metadata.etag = etag.clone();
                }
                metadata = recover_source_version_metadata(client, object, metadata).await;
                if let Some(cache) = policy.cache() {
                    if metadata.last_modified.is_some() || metadata.etag.is_some() {
                        let _ = cache.store_body_with_metadata(
                            &cache_key,
                            &body.etag,
                            &body.bytes,
                            Some(&metadata),
                        );
                    }
                }
                return String::from_utf8(body.bytes)
                    .map(|contents| (contents, metadata))
                    .map_err(|_| {
                        input_error(
                            &format!("s3://{}/{}", object.bucket, object.key),
                            "cached object contents are not valid UTF-8",
                        )
                    });
            }
            let response = client
                .get_object()
                .bucket(&object.bucket)
                .key(&object.key)
                .if_none_match(
                    etag.as_deref()
                        .expect("cached current object bodies require an ETag"),
                )
                .send()
                .await;
            match response {
                Err(error) if is_not_modified(&error) => {
                    let mut source_version = body.source_version.unwrap_or(crate::SourceVersion {
                        version_id: None,
                        last_modified: None,
                        etag: None,
                    });
                    source_version.last_modified = source_version
                        .last_modified
                        .or_else(|| last_modified.map(str::to_string));
                    source_version.etag = source_version.etag.or(etag.clone());
                    source_version =
                        recover_source_version_metadata(client, object, source_version).await;
                    let cache_etag = body.etag.clone();
                    let contents = String::from_utf8(body.bytes).map_err(|_| {
                        input_error(
                            &format!("s3://{}/{}", object.bucket, object.key),
                            "cached object contents are not valid UTF-8",
                        )
                    })?;
                    if let Some(cache) = policy.cache() {
                        if source_version.last_modified.is_some() || source_version.etag.is_some() {
                            let _ = cache.store_body_with_metadata(
                                &cache_key,
                                etag.as_deref().unwrap_or(&cache_etag),
                                contents.as_bytes(),
                                Some(&source_version),
                            );
                        }
                    }
                    return Ok((contents, source_version));
                }
                Ok(response) => {
                    let response_etag = response.e_tag().map(str::to_string);
                    let mut source_version = crate::SourceVersion {
                        version_id: response.version_id().map(str::to_string),
                        last_modified: response
                            .last_modified()
                            .and_then(|timestamp| timestamp.fmt(Format::DateTimeWithOffset).ok()),
                        etag: response_etag.clone(),
                    };
                    let bytes = response
                        .body
                        .collect()
                        .await
                        .map_err(|_| {
                            input_error(
                                &format!("s3://{}/{}", object.bucket, object.key),
                                "failed to read object body",
                            )
                        })?
                        .into_bytes();
                    source_version.last_modified = source_version
                        .last_modified
                        .or_else(|| last_modified.map(str::to_string));
                    source_version =
                        recover_source_version_metadata(client, object, source_version).await;
                    if let (Some(cache), Some(etag)) = (policy.cache(), response_etag.as_deref()) {
                        let refreshed_key = object_cache_key(client, object, None, Some(etag));
                        let _ = cache.store_body_with_metadata(
                            &refreshed_key,
                            etag,
                            &bytes,
                            Some(&source_version),
                        );
                        let pointer_key = object_cache_key(client, object, None, None);
                        let _ = cache.store("current-object-etags", &pointer_key, &etag);
                    }
                    let contents = String::from_utf8(bytes.to_vec()).map_err(|_| {
                        input_error(
                            &format!("s3://{}/{}", object.bucket, object.key),
                            "object contents are not valid UTF-8",
                        )
                    })?;
                    return Ok((contents, source_version));
                }
                Err(error) => return Err(get_object_error(object, &error)),
            }
        }
    }

    let mut request = client.get_object().bucket(&object.bucket).key(&object.key);
    if let Some(version_id) = version_id {
        request = request.version_id(version_id);
    }
    let response = request
        .send()
        .await
        .map_err(|error| get_object_error(object, &error))?;
    let response_etag = response.e_tag().map(str::to_string);
    let response_version_id = response.version_id().map(str::to_string);
    let response_last_modified = response
        .last_modified()
        .and_then(|timestamp| timestamp.fmt(Format::DateTimeWithOffset).ok());
    let bytes = response
        .body
        .collect()
        .await
        .map_err(|_| {
            input_error(
                &format!("s3://{}/{}", object.bucket, object.key),
                "failed to read object body",
            )
        })?
        .into_bytes();
    let mut source_version = crate::SourceVersion {
        version_id: response_version_id.or_else(|| version_id.map(str::to_string)),
        last_modified: response_last_modified.or_else(|| last_modified.map(str::to_string)),
        etag: response_etag.clone().or(etag.clone()),
    };
    if version_id.is_none() {
        source_version = recover_source_version_metadata(client, object, source_version).await;
    }
    if let Some(cache) = policy.cache() {
        if let Some(etag) = response_etag.as_deref().or(etag.as_deref()) {
            let cache_key = object_cache_key(client, object, version_id, Some(etag));
            let _ = cache.store_body_with_metadata(&cache_key, etag, &bytes, Some(&source_version));
            if version_id.is_none() {
                let pointer_key = object_cache_key(client, object, None, None);
                let _ = cache.store("current-object-etags", &pointer_key, &etag);
            }
        } else if let Some(version_id) = version_id {
            let cache_key = object_cache_key(client, object, Some(version_id), None);
            let _ = cache.store_body_with_metadata(&cache_key, "", &bytes, Some(&source_version));
        }
    }
    let contents = String::from_utf8(bytes.to_vec()).map_err(|_| {
        input_error(
            &format!("s3://{}/{}", object.bucket, object.key),
            "object contents are not valid UTF-8",
        )
    })?;
    Ok((contents, source_version))
}

fn object_cache_key(
    client: &Client,
    object: &S3Object,
    version_id: Option<&str>,
    etag: Option<&str>,
) -> Vec<u8> {
    let identity = (
        2_u8,
        client.config().region().map(|region| region.as_ref()),
        std::env::var("AWS_ENDPOINT_URL_S3")
            .or_else(|_| std::env::var("AWS_ENDPOINT_URL"))
            .unwrap_or_default(),
        std::env::var("AWS_PROFILE").unwrap_or_default(),
        std::env::var("AWS_SHARED_CREDENTIALS_FILE").unwrap_or_default(),
        object.bucket.as_str(),
        object.key.as_str(),
        version_id,
        if version_id.is_some() { None } else { etag },
    );
    serde_json::to_vec(&identity).expect("serializing cache identity cannot fail")
}

fn is_not_modified<E>(
    error: &aws_smithy_runtime_api::client::result::SdkError<E, SmithyResponse>,
) -> bool {
    error
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 304)
}

fn get_object_error<E>(
    object: &S3Object,
    error: &aws_smithy_runtime_api::client::result::SdkError<E, SmithyResponse>,
) -> TfdiffError
where
    E: aws_smithy_types::error::metadata::ProvideErrorMetadata + std::error::Error + 'static,
{
    let delete_marker = error
        .raw_response()
        .is_some_and(|response| response.headers().get("x-amz-delete-marker") == Some("true"));
    let message = if delete_marker {
        "selected S3 version is a delete marker".to_string()
    } else {
        format!("GetObject failed: {}", safe_sdk_error(error))
    };
    input_error(&format!("s3://{}/{}", object.bucket, object.key), &message)
}

/// An AWS error text as one terminal-safe line of at most 300 characters.
fn aws_text(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = crate::formatter::terminal::sanitize_terminal(&line);
    if line.chars().count() > 300 {
        format!("{}…", line.chars().take(300).collect::<String>())
    } else {
        line
    }
}

/// Why AWS credentials could not be loaded, if that is what made a request fail, followed by
/// the innermost error AWS reported (for example the SSO token error).
fn credentials_failure(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    use aws_credential_types::provider::error::CredentialsError;

    let mut current = Some(error);
    let mut credentials = None;
    let mut text = String::new();
    let mut cause = String::new();
    while let Some(error) = current {
        cause = error.to_string();
        if let Some(error) = error.downcast_ref::<CredentialsError>() {
            credentials.get_or_insert(error);
        }
        text.push_str(&error.to_string().to_lowercase());
        text.push('\n');
        current = error.source();
    }
    let credentials = credentials?;
    let profile = std::env::var("AWS_PROFILE")
        .ok()
        .filter(|profile| !profile.is_empty())
        .map(|profile| format!(" --profile {profile}"))
        .unwrap_or_default();
    let sso = text.contains("sso") || text.contains("token has expired");
    let hint = match credentials {
        _ if sso => format!(
            "AWS SSO login has expired or is missing; run `aws sso login{profile}` and try again"
        ),
        CredentialsError::CredentialsNotLoaded(_) => {
            "no AWS credentials found; set AWS_PROFILE or configure credentials".to_string()
        }
        CredentialsError::ProviderTimedOut(_) => {
            "loading AWS credentials timed out".to_string()
        }
        CredentialsError::InvalidConfiguration(_) => format!(
            "the AWS credentials configuration is invalid; check the profile{profile} in ~/.aws/config"
        ),
        _ => format!(
            "AWS credentials could not be loaded; check that you are logged in (for example `aws sso login{profile}`)"
        ),
    };
    let cause = aws_text(&cause);
    Some(if cause.is_empty() {
        hint
    } else {
        format!("{hint} (AWS: {cause})")
    })
}

/// The credentials failure behind an SDK error; such errors are fatal, not partial results.
fn sdk_credentials_failure<E, R>(
    error: &aws_smithy_runtime_api::client::result::SdkError<E, R>,
) -> Option<String>
where
    E: std::error::Error + 'static,
    R: std::fmt::Debug + 'static,
{
    credentials_failure(error)
}

fn safe_sdk_error<E, R>(error: &aws_smithy_runtime_api::client::result::SdkError<E, R>) -> String
where
    E: aws_smithy_types::error::metadata::ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug + 'static,
{
    use aws_smithy_types::error::metadata::ProvideErrorMetadata;

    if let Some(message) = sdk_credentials_failure(error) {
        return message;
    }
    // Service messages are not echoed: they can carry parts of the requested object.
    if let Some(code) = error.code() {
        return format!("service error ({code})");
    }
    match error {
        aws_smithy_runtime_api::client::result::SdkError::DispatchFailure(_) => {
            "request could not be dispatched".to_string()
        }
        aws_smithy_runtime_api::client::result::SdkError::TimeoutError(_) => {
            "request timed out".to_string()
        }
        aws_smithy_runtime_api::client::result::SdkError::ConstructionFailure(_) => {
            "request could not be constructed".to_string()
        }
        aws_smithy_runtime_api::client::result::SdkError::ResponseError(_) => {
            "invalid service response".to_string()
        }
        aws_smithy_runtime_api::client::result::SdkError::ServiceError(_) => {
            "service error".to_string()
        }
        _ => "AWS request failed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_sso_token_names_the_login_and_keeps_the_aws_message() {
        let error = aws_credential_types::provider::error::CredentialsError::provider_error(
            "Error when retrieving token from sso: Token has expired and refresh failed",
        );
        let message = credentials_failure(&error).expect("a credentials failure");
        assert!(
            message.starts_with("AWS SSO login has expired or is missing; run `aws sso login"),
            "{message}"
        );
        assert!(
            message.ends_with(
                "(AWS: Error when retrieving token from sso: Token has expired and refresh failed)"
            ),
            "{message}"
        );
    }

    #[test]
    fn other_errors_are_no_credentials_failure() {
        let error = std::io::Error::other("connection reset");
        assert!(credentials_failure(&error).is_none());
    }

    #[test]
    fn versioning_status_result_accepts_enabled() {
        assert!(versioning_status_result("b", Some(&BucketVersioningStatus::Enabled)).is_ok());
    }

    #[test]
    fn versioning_status_result_rejects_suspended_and_unset() {
        let suspended =
            versioning_status_result("b", Some(&BucketVersioningStatus::Suspended)).unwrap_err();
        assert!(suspended.to_string().contains(
            "--compact requires S3 bucket versioning to be Enabled; bucket versioning status is Suspended"
        ));
        let unset = versioning_status_result("b", None).unwrap_err();
        assert!(unset.to_string().contains(
            "--compact requires S3 bucket versioning to be Enabled; versioning has never been enabled on this bucket"
        ));
    }

    #[test]
    fn versioning_status_result_mentions_unknown_value() {
        let status = BucketVersioningStatus::from("Weird");
        let error = versioning_status_result("b", Some(&status)).unwrap_err();
        assert!(error.to_string().contains("status is Weird"));
    }

    #[test]
    fn parses_current_and_explicit_versions() {
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/key/path.tfstate")).unwrap(),
            InputSource::S3(S3Object {
                bucket: "bucket".into(),
                key: "key/path.tfstate".into(),
                selector: VersionSelector::Current,
            })
        );
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/a%40b.tfstate@v%2F1")).unwrap(),
            InputSource::S3(S3Object {
                bucket: "bucket".into(),
                key: "a@b.tfstate".into(),
                selector: VersionSelector::Version("v/1".into()),
            })
        );
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/key@%48EAD")).unwrap(),
            InputSource::S3(S3Object {
                bucket: "bucket".into(),
                key: "key".into(),
                selector: VersionSelector::Version("HEAD".into()),
            })
        );
    }

    #[test]
    fn parses_s3_prefix_and_glob_inputs_without_decoding_escaped_metacharacters_as_patterns() {
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/{dev,prod}/")).unwrap(),
            InputSource::S3Inventory(S3Inventory {
                bucket: "bucket".into(),
                selector: InventorySelector::Glob {
                    prefix: "".into(),
                    pattern: "{dev,prod}/".into(),
                },
            })
        );
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/dev/")).unwrap(),
            InputSource::S3Inventory(S3Inventory {
                bucket: "bucket".into(),
                selector: InventorySelector::Prefix("dev/".into()),
            })
        );
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/{dev,prod}/**/")).unwrap(),
            InputSource::S3Inventory(S3Inventory {
                bucket: "bucket".into(),
                selector: InventorySelector::Glob {
                    prefix: "".into(),
                    pattern: "{dev,prod}/**/".into(),
                },
            })
        );
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/dev/**/*.tfstate")).unwrap(),
            InputSource::S3Inventory(S3Inventory {
                bucket: "bucket".into(),
                selector: InventorySelector::Glob {
                    prefix: "dev/".into(),
                    pattern: "dev/**/*.tfstate".into(),
                },
            })
        );
        assert_eq!(
            InputSource::parse(Path::new("s3://bucket/{dev,prod}/state%2A.tfstate")).unwrap(),
            InputSource::S3Inventory(S3Inventory {
                bucket: "bucket".into(),
                selector: InventorySelector::Glob {
                    prefix: "".into(),
                    pattern: r"{dev,prod}/state\*.tfstate".into(),
                },
            })
        );
        let pattern = GlobBuilder::new(r"{dev,prod}/state\*.tfstate")
            .literal_separator(true)
            .build()
            .unwrap()
            .compile_matcher();
        assert!(pattern.is_match("dev/state*.tfstate"));
        assert!(!pattern.is_match("prod/state-any.tfstate"));
        assert!(InputSource::parse(Path::new("s3://bucket/dev/*@v1")).is_err());
        assert!(matches!(
            InputSource::parse(Path::new("s3://bucket/state%2A.tfstate")).unwrap(),
            InputSource::S3(S3Object { key, .. }) if key == "state*.tfstate"
        ));
    }

    #[test]
    fn parses_s3_regex_inputs_in_regex_mode() {
        use crate::pattern::PatternMode;
        let parse = |uri: &str| InputSource::parse_with_mode(Path::new(uri), PatternMode::Regex);
        assert_eq!(
            parse(r"s3://bucket/env/(dev|prod)/.*\.tfstate").unwrap(),
            InputSource::S3Inventory(S3Inventory {
                bucket: "bucket".into(),
                selector: InventorySelector::Regex {
                    prefix: "env/".into(),
                    pattern: r"env/(dev|prod)/.*\.tfstate".into(),
                },
            })
        );
        assert!(matches!(
            parse("s3://bucket/[a-z]+/state").unwrap(),
            InputSource::S3Inventory(S3Inventory {
                selector: InventorySelector::Regex { prefix, .. },
                ..
            }) if prefix.is_empty()
        ));
        // A dot alone keeps a key an exact object.
        assert!(matches!(
            parse("s3://bucket/env/app.tfstate").unwrap(),
            InputSource::S3(S3Object { key, .. }) if key == "env/app.tfstate"
        ));
        assert!(parse("s3://bucket/env/").unwrap().is_inventory());
        let error = parse("s3://bucket/env/(dev").unwrap_err();
        assert!(
            error.to_string().contains("invalid S3 regex pattern"),
            "{error}"
        );
        // In glob mode the same braces are glob syntax.
        assert!(matches!(
            InputSource::parse(Path::new("s3://bucket/{dev,prod}/a.tfstate")).unwrap(),
            InputSource::S3Inventory(S3Inventory {
                selector: InventorySelector::Glob { .. },
                ..
            })
        ));
        let regex = crate::pattern::anchored_regex(r"env/(dev|prod)/.*\.tfstate").unwrap();
        let matcher = KeyMatcher::Regex(regex);
        assert!(key_matches("env/dev/a/b.tfstate", "env/", Some(&matcher)));
        assert!(!key_matches("env/qa/b.tfstate", "env/", Some(&matcher)));
        assert!(!key_matches(
            "env/dev/b.tfstate.backup",
            "env/",
            Some(&matcher)
        ));
    }

    #[test]
    fn inventory_ties_use_is_latest_then_preserve_source_order() {
        let mut history = [
            S3InventoryVersion {
                version_id: Some("older-first-in-response".into()),
                last_modified: Some("2026-01-01T00:00:00.000000100Z".into()),
                is_delete_marker: false,
                is_latest: false,
                size: Some(1),
                etag: None,
            },
            S3InventoryVersion {
                version_id: Some("latest-marker".into()),
                last_modified: Some("2026-01-01T00:00:00.000000100Z".into()),
                is_delete_marker: true,
                is_latest: true,
                size: None,
                etag: None,
            },
            S3InventoryVersion {
                version_id: Some("newer-subsecond".into()),
                last_modified: Some("2026-01-01T00:00:00.000000200Z".into()),
                is_delete_marker: false,
                is_latest: false,
                size: Some(1),
                etag: None,
            },
        ];
        history.sort_by(compare_inventory_versions);
        assert_eq!(
            history
                .iter()
                .map(|event| event.version_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "newer-subsecond",
                "latest-marker",
                "older-first-in-response"
            ]
        );
    }

    #[test]
    fn glob_matches_hierarchical_keys_and_brace_alternatives() {
        let pattern = GlobBuilder::new("dev/{blue,green}/**/*.tfstate")
            .literal_separator(true)
            .build()
            .unwrap()
            .compile_matcher();
        assert!(pattern.is_match("dev/blue/terraform.tfstate"));
        assert!(pattern.is_match("dev/green/nested/terraform.tfstate"));
        assert!(!pattern.is_match("dev/red/terraform.tfstate"));
        assert!(!pattern.is_match("dev/blue/file.txt"));
        assert!(key_matches("dev/state.tfstate", "dev/", None));
        assert!(!key_matches("devops/state.tfstate", "dev/", None));
    }

    #[test]
    fn validates_uri_without_changing_local_paths() {
        for uri in [
            "s3://bucket",
            "s3:///key",
            "s3://bucket/key@",
            "s3://bucket/a%Q0",
        ] {
            assert!(InputSource::parse(Path::new(uri)).is_err(), "{uri}");
        }
        assert_eq!(
            InputSource::parse(Path::new("local@HEAD~2.txt")).unwrap(),
            InputSource::Local(PathBuf::from("local@HEAD~2.txt"))
        );
    }
}
