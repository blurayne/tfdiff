use crate::input::{
    list_s3_inventory_with_config, S3CachePolicy, S3ClientConfig, S3Inventory, S3VersionReader,
};
use crate::progress::ProgressReporter;
use crate::tfstate_branches::BranchReport;
use crate::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub lineage: Option<String>,
    pub serial: Option<u64>,
    pub resource_ids: BTreeSet<String>,
    /// `type|identity` (id, else arn, else name) of every managed resource instance, independent of module and name.
    pub managed_resources: BTreeSet<String>,
    pub outputs: BTreeMap<String, serde_json::Value>,
}

pub fn fingerprint(body: &[u8]) -> std::result::Result<Fingerprint, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "unreadable state".to_string())?;
    let root = value.as_object().ok_or("unreadable state")?;
    let mut resource_ids = BTreeSet::new();
    let mut managed_resources = BTreeSet::new();
    match root.get("resources") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(resources)) => {
            for resource in resources {
                let resource = resource.as_object().ok_or("unreadable state")?;
                let field = |name: &str| resource.get(name).map(scalar).unwrap_or_default();
                let prefix = format!(
                    "{}|{}|{}|{}",
                    field("mode"),
                    field("module"),
                    field("type"),
                    field("name")
                );
                let instances = match resource.get("instances") {
                    None | Some(serde_json::Value::Null) => &[][..],
                    Some(serde_json::Value::Array(instances)) => instances,
                    Some(_) => return Err("unreadable state".to_string()),
                };
                for instance in instances {
                    let index_key = instance.get("index_key").map(scalar).unwrap_or_default();
                    let attribute = |name: &str| {
                        instance
                            .get("attributes")
                            .and_then(|attributes| attributes.get(name))
                    };
                    let id = attribute("id").map(scalar).unwrap_or_default();
                    if field("mode") == "managed" {
                        if let Some((_, identity)) = crate::migrations::identity_value(attribute) {
                            managed_resources.insert(format!("{}|{identity}", field("type")));
                        }
                    }
                    resource_ids.insert(format!("{prefix}|{index_key}|{id}"));
                }
            }
        }
        Some(_) => return Err("unreadable state".to_string()),
    }
    let outputs = match root.get("outputs") {
        None | Some(serde_json::Value::Null) => BTreeMap::new(),
        Some(serde_json::Value::Object(outputs)) => outputs
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        Some(_) => return Err("unreadable state".to_string()),
    };
    Ok(Fingerprint {
        lineage: root
            .get("lineage")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        serial: root.get("serial").and_then(|value| value.as_u64()),
        resource_ids,
        managed_resources,
        outputs,
    })
}

/// SHA-256 over the ordered managed resource identities; `None` when there are none.
pub fn resource_hash(fingerprint: &Fingerprint) -> Option<String> {
    (!fingerprint.managed_resources.is_empty()).then(|| {
        let joined: Vec<&str> = fingerprint
            .managed_resources
            .iter()
            .map(String::as_str)
            .collect();
        crate::cache::sha256_hex(joined.join("\n").as_bytes())
    })
}

/// Terminal form of a resource hash: 16 characters unless the full hash is requested.
pub(crate) fn display_hash(hash: &str, full: bool) -> &str {
    if full {
        hash
    } else {
        &hash[..hash.len().min(16)]
    }
}

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VersionStatus {
    Current,
    Deprecated,
    Kept,
    Deleted,
}

impl VersionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "CURRENT",
            Self::Deprecated => "DEPRECATED",
            Self::Kept => "KEPT",
            Self::Deleted => "DELETED",
        }
    }
}

pub struct VersionInput {
    pub version_id: String,
    pub last_modified: Option<String>,
    pub is_delete_marker: bool,
    pub is_latest: bool,
    /// `None` for delete markers; otherwise the body's SHA-256 hex and fingerprint, or why it could not be read.
    pub content: Option<std::result::Result<(String, Fingerprint), String>>,
}

pub struct KeyHistory {
    pub key: String,
    pub versions: Vec<VersionInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MatchedVersion {
    pub key: String,
    pub version_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClassifiedVersion {
    pub version_id: String,
    pub last_modified: Option<String>,
    pub status: VersionStatus,
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_to: Option<MatchedVersion>,
    /// See [`resource_hash`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeyDuplicates {
    pub key: String,
    pub versions: Vec<ClassifiedVersion>,
}

pub(crate) fn latest_index(versions: &[VersionInput]) -> usize {
    versions
        .iter()
        .position(|version| version.is_latest)
        .unwrap_or(0)
}

pub(crate) fn is_deleted(versions: &[VersionInput]) -> bool {
    versions
        .get(latest_index(versions))
        .is_some_and(|version| version.is_delete_marker)
}

struct Readable<'a> {
    hash: &'a str,
    /// `None` when the state has no managed resources, so empty states never match each other.
    resources: Option<String>,
}

struct Node<'a> {
    key: &'a str,
    position: usize,
    version: &'a VersionInput,
    key_deleted: bool,
    /// LastModified of the key's latest entry; decides which key a state moved to.
    key_latest: Option<chrono::DateTime<chrono::FixedOffset>>,
    is_latest: bool,
    readable: Option<Readable<'a>>,
}

fn resources_key(fingerprint: &Fingerprint) -> Option<String> {
    (!fingerprint.managed_resources.is_empty()).then(|| {
        serde_json::to_string(&fingerprint.managed_resources).expect("resource set serializes")
    })
}

fn timestamp(node: &Node) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    parsed_time(node.version.last_modified.as_deref())
}

/// A live key's latest version is only superseded by a key that was written to more recently,
/// so two live keys can never mark each other's latest version as removable.
fn may_supersede(candidate: &Node, node: &Node) -> bool {
    node.key_deleted
        || !node.is_latest
        || matches!((candidate.key_latest, node.key_latest), (Some(candidate), Some(of)) if candidate > of)
}

/// The latest version of a live key, holding managed resources.
fn is_head(node: &Node) -> bool {
    node.is_latest
        && !node.key_deleted
        && node
            .readable
            .as_ref()
            .is_some_and(|readable| readable.resources.is_some())
}

fn is_newer(candidate: &Node, than: &Node) -> bool {
    match (timestamp(candidate), timestamp(than)) {
        (Some(candidate), Some(than)) => candidate > than,
        _ => candidate.key == than.key && candidate.position < than.position,
    }
}

pub fn classify_histories(histories: &[KeyHistory]) -> Vec<KeyDuplicates> {
    let mut nodes = Vec::new();
    for history in histories {
        let key_deleted = is_deleted(&history.versions);
        let latest = latest_index(&history.versions);
        let key_latest = history
            .versions
            .get(latest)
            .and_then(|version| parsed_time(version.last_modified.as_deref()));
        for (position, version) in history.versions.iter().enumerate() {
            let readable = match &version.content {
                Some(Ok((hash, fingerprint))) => Some(Readable {
                    hash,
                    resources: resources_key(fingerprint),
                }),
                _ => None,
            };
            nodes.push(Node {
                key: &history.key,
                position,
                version,
                key_deleted,
                key_latest,
                is_latest: position == latest,
                readable,
            });
        }
    }

    let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_resources: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if let Some(readable) = &node.readable {
            by_hash.entry(readable.hash).or_default().push(index);
            if let Some(resources) = &readable.resources {
                by_resources.entry(resources).or_default().push(index);
            }
        }
    }

    let classify = |index: usize| -> (VersionStatus, Option<String>, Option<usize>) {
        let node = &nodes[index];
        if node.version.is_delete_marker {
            return (VersionStatus::Deleted, None, None);
        }
        if let Some(readable) = &node.readable {
            let describe =
                |other: usize| format!("{} @{}", nodes[other].key, nodes[other].version.version_id);
            // Of identical heads the oldest is the original and the newer ones are its copies.
            if is_head(node) {
                let original = by_hash
                    .get(readable.hash)
                    .into_iter()
                    .flatten()
                    .filter(|other| {
                        let other = &nodes[**other];
                        other.key != node.key && is_head(other) && is_newer(node, other)
                    })
                    .min_by_key(|other| timestamp(&nodes[**other]))
                    .copied();
                if let Some(other) = original {
                    return (
                        VersionStatus::Deprecated,
                        Some(format!("copy of {}", describe(other))),
                        Some(other),
                    );
                }
            }
            // Only copies in another key count: a key's own version history is always kept.
            // An original head is never superseded by its own identical copies.
            let live_newer = |other: &&usize| {
                let other = &nodes[**other];
                other.key != node.key
                    && !other.key_deleted
                    && may_supersede(other, node)
                    && is_newer(other, node)
                    && !(is_head(node)
                        && is_head(other)
                        && other.readable.as_ref().map(|other| other.hash) == Some(readable.hash))
            };
            // The oldest qualifying copy is where the state first appeared after the move.
            let earliest = |candidates: Option<&Vec<usize>>| {
                candidates
                    .into_iter()
                    .flatten()
                    .filter(live_newer)
                    .min_by_key(|other| timestamp(&nodes[**other]))
                    .copied()
            };
            if let Some(other) = earliest(by_hash.get(readable.hash)) {
                return (
                    VersionStatus::Deprecated,
                    Some(format!("identical to {}", describe(other))),
                    Some(other),
                );
            }
            if let Some(other) = earliest(
                readable
                    .resources
                    .as_deref()
                    .and_then(|resources| by_resources.get(resources)),
            ) {
                return (
                    VersionStatus::Deprecated,
                    Some(format!("same resources as {}", describe(other))),
                    Some(other),
                );
            }
        }
        if !node.key_deleted && node.is_latest {
            return (VersionStatus::Current, None, None);
        }
        let reason = if node.key_deleted {
            "key is deleted; history retained"
        } else if node.readable.is_none() {
            "unreadable state"
        } else {
            "no matching copy in another key"
        };
        (VersionStatus::Kept, Some(reason.to_string()), None)
    };

    let mut outcomes = (0..nodes.len()).map(classify);
    histories
        .iter()
        .map(|history| KeyDuplicates {
            key: history.key.clone(),
            versions: history
                .versions
                .iter()
                .map(|version| {
                    let (status, reason, target) =
                        outcomes.next().expect("one outcome per version");
                    ClassifiedVersion {
                        version_id: version.version_id.clone(),
                        last_modified: version.last_modified.clone(),
                        status,
                        reason,
                        matched_to: target.map(|target| MatchedVersion {
                            key: nodes[target].key.to_string(),
                            version_id: nodes[target].version.version_id.clone(),
                        }),
                        resource_hash: match &version.content {
                            Some(Ok((_, fingerprint))) => resource_hash(fingerprint),
                            _ => None,
                        },
                    }
                })
                .collect(),
        })
        .collect()
}

fn is_lock_file(key: &str) -> bool {
    key.rsplit('/')
        .next()
        .is_some_and(|name| name.contains("tflock"))
}

pub async fn build_duplicate_report(
    source: &S3Inventory,
    cache_policy: &S3CachePolicy,
    s3_config: &S3ClientConfig,
    line_progress: bool,
    color: bool,
) -> Result<BranchReport> {
    let scan = list_s3_inventory_with_config(source, s3_config).await?;
    let reader = S3VersionReader::new_with_config(cache_policy, s3_config).await?;
    let objects: Vec<_> = scan
        .objects
        .into_iter()
        .filter(|object| !is_lock_file(&object.key))
        .collect();
    let reads: usize = objects
        .iter()
        .map(|object| {
            object
                .history
                .iter()
                .filter(|version| !version.is_delete_marker)
                .count()
        })
        .sum();
    let progress = ProgressReporter::new(
        reads,
        "S3 duplicates",
        &format!("reading {reads} state version(s)"),
        line_progress,
        color,
    );
    let mut histories = Vec::with_capacity(objects.len());
    for object in objects {
        let mut versions = Vec::with_capacity(object.history.len());
        for version in &object.history {
            let version_id = version.version_id.clone().unwrap_or_else(|| "null".into());
            let content = if version.is_delete_marker {
                None
            } else {
                if let Some(progress) = progress.as_ref() {
                    progress.start(&format!("{} @{version_id}", object.key));
                }
                let content = match reader.load_selected(source, &object.key, version).await {
                    Ok(loaded) => fingerprint(loaded.contents.as_bytes()).map(|fingerprint| {
                        (
                            crate::cache::sha256_hex(loaded.contents.as_bytes()),
                            fingerprint,
                        )
                    }),
                    Err(_) => Err("unreadable state".to_string()),
                };
                if let Some(progress) = progress.as_ref() {
                    progress.complete(&format!("{} @{version_id}", object.key));
                }
                Some(content)
            };
            versions.push(VersionInput {
                version_id,
                last_modified: version.last_modified.clone(),
                is_delete_marker: version.is_delete_marker,
                is_latest: version.is_latest,
                content,
            });
        }
        histories.push(KeyHistory {
            key: object.key,
            versions,
        });
    }
    let mut report = crate::tfstate_branches::build(&histories, classify_histories(&histories));
    report.bucket = source.bucket.clone();
    report.errors = scan.errors;
    if let Some(progress) = progress {
        progress.finish(&format!(
            "read {reads} version(s), classified {} key(s)",
            report.keys.len()
        ));
    }
    Ok(report)
}

pub(crate) fn parsed_time(value: Option<&str>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    value.and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
}

fn newest(versions: &[ClassifiedVersion]) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    versions
        .iter()
        .filter_map(|version| parsed_time(version.last_modified.as_deref()))
        .max()
}

pub fn sort_newest_first(keys: &mut [KeyDuplicates]) {
    for key in keys.iter_mut() {
        key.versions.sort_by(|left, right| {
            parsed_time(right.last_modified.as_deref())
                .cmp(&parsed_time(left.last_modified.as_deref()))
        });
    }
    keys.sort_by_key(|key| std::cmp::Reverse(newest(&key.versions)));
}

pub(crate) fn format_line(
    bucket: &str,
    key: &str,
    version: &ClassifiedVersion,
    show_datetime: bool,
    full_hash: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let hash = version
        .resource_hash
        .as_deref()
        .map(|hash| format!(" #{}", display_hash(hash, full_hash)))
        .unwrap_or_default();
    let mut line = format!(
        "s3://{bucket}/{key} @{}{hash} {}",
        version.version_id,
        version.status.as_str()
    );
    if let Some(timestamp) = &version.last_modified {
        let when = if show_datetime {
            crate::formatter::terminal::sanitize_terminal(timestamp)
        } else {
            crate::formatter::terminal::format_state_age(timestamp, now)
        };
        line.push(' ');
        line.push_str(&when);
    }
    if matches!(
        version.status,
        VersionStatus::Kept | VersionStatus::Deprecated
    ) {
        if let Some(reason) = &version.reason {
            line.push_str(&format!(" ({reason})"));
        }
    }
    line
}
