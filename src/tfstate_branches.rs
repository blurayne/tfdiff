//! Git-like branch history of the state files under an S3 prefix.
//!
//! Resources are followed by identity (`type|id`, falling back to arn and name) through the
//! versions of every key. A resource that shows up in a key after another key stopped holding
//! it was moved there, which yields edges between keys: a pure move, a split into several keys
//! or a merge into a bigger one. A key whose last live snapshot is fully contained in newer
//! versions of other live keys is OBSOLETE, or DEPRECATED when it was written again after the
//! move. Of identical live copies the newer ones are DEPRECATED. A live key without any managed
//! resources is EMPTY, and a key whose latest entry is a delete marker is always DELETED. The
//! terminal output groups keys linked by moves into a status table and a history of
//! the moves. Everything here is display-only and never prints resource ids or attributes, only
//! keys, version ids, counts and timestamps.

use crate::tfstate_duplicates::{
    display_hash, format_line, is_deleted, latest_index, parsed_time, resource_hash,
    sort_newest_first, ClassifiedVersion, KeyDuplicates, KeyHistory, MatchedVersion, VersionInput,
};
use serde::{Serialize, Serializer};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

type Moment = chrono::DateTime<chrono::FixedOffset>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObsoleteKind {
    /// The snapshot's bytes exist in a newer version of another key.
    MovedIdentical,
    /// A newer version of another key holds exactly the same resources.
    MovedSameIds,
    /// One other key holds every resource, together with more.
    Merged { into: String },
    /// Several other keys are needed to hold every resource.
    Split { into: usize },
}

impl ObsoleteKind {
    /// Short form for the JSON `detail`.
    fn detail(&self) -> String {
        match self {
            Self::MovedIdentical => "moved, identical".to_string(),
            Self::MovedSameIds => "moved, same resource ids".to_string(),
            Self::Merged { into } => format!("merged into {into}"),
            Self::Split { into } => format!("split into {into} states"),
        }
    }

    /// Plain words for the terminal table.
    fn reason(&self) -> String {
        match self {
            Self::MovedIdentical => "identical copy exists in a newer file".to_string(),
            Self::MovedSameIds => "same resources exist in a newer file".to_string(),
            Self::Merged { into } => format!("all resources merged into {into}"),
            Self::Split { into } => format!("resources split into {into} other files"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeprecatedKind {
    /// The last live snapshot has the same bytes as the last live snapshot of an older live
    /// key, which stays ACTIVE: of two identical copies the newer one is deprecated.
    CopyOf { original: String },
    /// Every resource lives on in newer versions of other live keys, but the key was written
    /// again after its resources had appeared there.
    WrittenAfterMove { kind: ObsoleteKind },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStatus {
    Active,
    /// A live key whose last live snapshot holds no managed resources, for example because the
    /// stack was destroyed; a candidate for removal.
    Empty,
    Obsolete {
        kind: ObsoleteKind,
    },
    Deprecated {
        kind: DeprecatedKind,
    },
    /// The latest entry is a delete marker; it wins over every other status. `elsewhere` of
    /// the `of` resources of the last live snapshot exist in newer versions of other live keys.
    Deleted {
        elsewhere: usize,
        of: usize,
    },
}

impl KeyStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Empty => "EMPTY",
            Self::Obsolete { .. } => "OBSOLETE",
            Self::Deprecated { .. } => "DEPRECATED",
            Self::Deleted { .. } => "DELETED",
        }
    }

    /// The parenthesised explanation of the status, if it has one.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Active => None,
            Self::Empty => Some("no managed resources, removal candidate".to_string()),
            Self::Obsolete { kind } => Some(kind.detail()),
            Self::Deprecated {
                kind: DeprecatedKind::CopyOf { original },
            } => Some(format!("copy of {original}")),
            Self::Deprecated {
                kind: DeprecatedKind::WrittenAfterMove { kind },
            } => Some(format!("{}, written after the move", kind.detail())),
            Self::Deleted { of: 0, .. } => None,
            Self::Deleted { elsewhere, of } => {
                Some(format!("{elsewhere}/{of} resources exist elsewhere"))
            }
        }
    }
}

impl Serialize for KeyStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeyNode {
    pub key: String,
    pub status: KeyStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// LastModified of the key's latest entry.
    pub last_modified: Option<String>,
    /// Managed resources in the key's last live snapshot.
    pub resources: usize,
    /// Version id of the last live snapshot, which `resources` and `resource_hash` describe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// The snapshot is the latest entry of an ACTIVE key: the tip of the history, not moved on
    /// to another file.
    pub head: bool,
    /// Hash over the ordered resource identities of the last live snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_hash: Option<String>,
    /// For an identical copy or the same resources elsewhere: the first matching version of
    /// each other key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<MatchedVersion>,
    pub versions: Vec<ClassifiedVersion>,
}

/// Resources that first appeared in `to` after `from` stopped holding them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Edge {
    /// The latest version of the source key that held any of the resources.
    pub from: MatchedVersion,
    /// The first version of the destination key that held any of the resources.
    pub to: MatchedVersion,
    /// LastModified of the `to` version.
    pub at: Option<String>,
    pub resources: usize,
    /// Managed resources in the `from` version.
    pub of: usize,
    /// The `to` bytes equal some version of the source key.
    pub identical: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BranchReport {
    pub bucket: String,
    pub keys: Vec<KeyNode>,
    pub edges: Vec<Edge>,
    pub errors: Vec<String>,
}

/// A readable (non-marker) version of a key.
struct Snapshot<'a> {
    position: usize,
    hash: &'a str,
    resources: &'a BTreeSet<String>,
    time: Option<Moment>,
    version: &'a VersionInput,
}

struct KeyView<'a> {
    history: &'a KeyHistory,
    deleted: bool,
    /// LastModified of the key's latest entry; decides which live key supersedes which.
    latest: Option<Moment>,
    readable: Vec<Snapshot<'a>>,
    /// Index into `readable` of the last live snapshot.
    snapshot: Option<usize>,
}

fn view(history: &KeyHistory) -> KeyView<'_> {
    let latest = latest_index(&history.versions);
    let readable: Vec<_> = history
        .versions
        .iter()
        .enumerate()
        .filter_map(|(position, version)| match &version.content {
            Some(Ok((hash, fingerprint))) => Some(Snapshot {
                position,
                hash,
                resources: &fingerprint.managed_resources,
                time: parsed_time(version.last_modified.as_deref()),
                version,
            }),
            _ => None,
        })
        .collect();
    let snapshot = readable
        .iter()
        .enumerate()
        .max_by_key(|(_, snapshot)| (snapshot.time, Reverse(snapshot.position)))
        .map(|(index, _)| index);
    KeyView {
        history,
        deleted: is_deleted(&history.versions),
        latest: history
            .versions
            .get(latest)
            .and_then(|version| parsed_time(version.last_modified.as_deref())),
        readable,
        snapshot,
    }
}

fn newer(candidate: Option<Moment>, than: Option<Moment>) -> bool {
    matches!((candidate, than), (Some(candidate), Some(than)) if candidate > than)
}

/// When a key first and last held one resource, as (time, index into `readable`).
#[derive(Clone, Copy)]
struct Span {
    first: (Moment, usize),
    last: (Moment, usize),
}

struct Flow {
    count: usize,
    from: (Moment, usize),
    to: (Moment, usize),
}

fn build_edges(views: &[KeyView]) -> Vec<Edge> {
    let mut seen: BTreeMap<&str, BTreeMap<usize, Span>> = BTreeMap::new();
    for (key, view) in views.iter().enumerate() {
        for (index, snapshot) in view.readable.iter().enumerate() {
            // Versions without a usable timestamp cannot be ordered against other keys.
            let Some(time) = snapshot.time else { continue };
            for resource in snapshot.resources {
                seen.entry(resource)
                    .or_default()
                    .entry(key)
                    .and_modify(|span| {
                        if time < span.first.0 {
                            span.first = (time, index);
                        }
                        if time > span.last.0 {
                            span.last = (time, index);
                        }
                    })
                    .or_insert(Span {
                        first: (time, index),
                        last: (time, index),
                    });
            }
        }
    }

    let mut flows: BTreeMap<(usize, usize), Flow> = BTreeMap::new();
    for holders in seen.values() {
        for (&child, span) in holders {
            // The parent is the other key that held the resource last before the child did.
            // Ranking by first sighting keeps two keys with the same instant from being each
            // other's parent. A copy whose source kept the resource for a while overlaps the
            // child; then the most recent earlier holder is the parent.
            let earlier = |&(&parent, other): &(&usize, &Span)| {
                parent != child && (other.first.0, parent) < (span.first.0, child)
            };
            let parent = holders
                .iter()
                .filter(|holder| earlier(holder) && holder.1.last.0 <= span.first.0)
                .max_by_key(|&(&parent, other)| (other.last.0, Reverse(parent)))
                .or_else(|| {
                    holders
                        .iter()
                        .filter(earlier)
                        .max_by_key(|&(&parent, other)| (other.first.0, Reverse(parent)))
                });
            let Some((&parent, other)) = parent else {
                continue;
            };
            flows
                .entry((parent, child))
                .and_modify(|flow| {
                    flow.count += 1;
                    if other.last.0 > flow.from.0 {
                        flow.from = other.last;
                    }
                    if span.first.0 < flow.to.0 {
                        flow.to = span.first;
                    }
                })
                .or_insert(Flow {
                    count: 1,
                    from: other.last,
                    to: span.first,
                });
        }
    }

    flows
        .into_iter()
        .map(|((parent, child), flow)| {
            let source = &views[parent];
            let from = &source.readable[flow.from.1];
            let to = &views[child].readable[flow.to.1];
            Edge {
                from: MatchedVersion {
                    key: source.history.key.clone(),
                    version_id: from.version.version_id.clone(),
                },
                to: MatchedVersion {
                    key: views[child].history.key.clone(),
                    version_id: to.version.version_id.clone(),
                },
                at: to.version.last_modified.clone(),
                resources: flow.count,
                of: from.resources.len(),
                identical: source
                    .readable
                    .iter()
                    .any(|candidate| candidate.hash == to.hash),
            }
        })
        .collect()
}

/// Another live key that holds some of a snapshot's resources in newer versions.
struct Destination<'a> {
    key: usize,
    covered: BTreeSet<&'a str>,
    /// The first newer version with the snapshot's bytes.
    identical: Option<MatchedVersion>,
    /// The first newer version with exactly the snapshot's resources.
    same_ids: Option<MatchedVersion>,
    /// When the key first held any of the snapshot's resources, in any version.
    first: Option<Moment>,
}

/// The last live snapshot of a live key, if it holds managed resources and is dated.
fn current<'v, 'a>(view: &'v KeyView<'a>) -> Option<&'v Snapshot<'a>> {
    let snapshot = &view.readable[view.snapshot?];
    (!view.deleted && !snapshot.resources.is_empty() && snapshot.time.is_some()).then_some(snapshot)
}

/// Live keys whose last live snapshots have the same bytes are copies of each other; the one
/// written first is the original. Returns the original of `index`'s copies, which may be
/// `index` itself, or `None` when no other live key holds the same bytes.
fn original(views: &[KeyView], index: usize) -> Option<usize> {
    let hash = current(&views[index])?.hash;
    let copies: Vec<usize> = (0..views.len())
        .filter(|&other| current(&views[other]).is_some_and(|snapshot| snapshot.hash == hash))
        .collect();
    (copies.len() > 1).then(|| {
        *copies
            .iter()
            .min_by_key(|&&copy| (current(&views[copy]).map(|snapshot| snapshot.time), copy))
            .expect("at least two copies")
    })
}

/// The status of a key, the number of managed resources in its last live snapshot, and the
/// versions of other keys that hold an identical copy or the same resources.
fn judge(views: &[KeyView], index: usize) -> (KeyStatus, usize, Vec<MatchedVersion>) {
    let view = &views[index];
    let fallback = |elsewhere: usize, of: usize| {
        if view.deleted {
            KeyStatus::Deleted { elsewhere, of }
        } else {
            KeyStatus::Active
        }
    };
    let Some(snapshot) = view.snapshot.map(|snapshot| &view.readable[snapshot]) else {
        return (fallback(0, 0), 0, Vec::new());
    };
    let of = snapshot.resources.len();
    // Empty snapshots hold nothing that could live on elsewhere.
    if of == 0 && !view.deleted {
        return (KeyStatus::Empty, 0, Vec::new());
    }
    // An undated snapshot cannot be compared with newer versions.
    if of == 0 || snapshot.time.is_none() {
        return (fallback(0, of), of, Vec::new());
    }
    // Of identical copies the newer ones are deprecated and the original is kept.
    let original = original(views, index);
    if let Some(original) = original.filter(|&original| original != index) {
        let source = &views[original];
        let matches = current(source)
            .map(|snapshot| MatchedVersion {
                key: source.history.key.clone(),
                version_id: snapshot.version.version_id.clone(),
            })
            .into_iter()
            .collect();
        let kind = DeprecatedKind::CopyOf {
            original: source.history.key.clone(),
        };
        return (KeyStatus::Deprecated { kind }, of, matches);
    }

    let mut destinations = Vec::new();
    for (other_index, other) in views.iter().enumerate() {
        if other_index == index || other.deleted {
            continue;
        }
        // A live key is only obsoleted by a key that was written to more recently, so two
        // live keys can never obsolete each other.
        if !view.deleted && !newer(other.latest, view.latest) {
            continue;
        }
        // The original is not made obsolete by its own copies.
        if original.is_some()
            && current(other).is_some_and(|candidate| candidate.hash == snapshot.hash)
        {
            continue;
        }
        let mut destination = Destination {
            key: other_index,
            covered: BTreeSet::new(),
            identical: None,
            same_ids: None,
            first: None,
        };
        for candidate in &other.readable {
            if candidate.time.is_some() && !snapshot.resources.is_disjoint(candidate.resources) {
                destination.first = match destination.first {
                    Some(first) if Some(first) <= candidate.time => Some(first),
                    _ => candidate.time,
                };
            }
            if !newer(candidate.time, snapshot.time) {
                continue;
            }
            destination.covered.extend(
                snapshot
                    .resources
                    .intersection(candidate.resources)
                    .map(String::as_str),
            );
        }
        // The other key's current snapshot when it matches, else its first newer match.
        let earliest = |matches: &dyn Fn(&Snapshot) -> bool| {
            let newer_match =
                |candidate: &&Snapshot| newer(candidate.time, snapshot.time) && matches(candidate);
            current(other)
                .filter(newer_match)
                .or_else(|| {
                    other
                        .readable
                        .iter()
                        .filter(newer_match)
                        .min_by_key(|candidate| candidate.time)
                })
                .map(|candidate| MatchedVersion {
                    key: other.history.key.clone(),
                    version_id: candidate.version.version_id.clone(),
                })
        };
        destination.identical = earliest(&|candidate| candidate.hash == snapshot.hash);
        destination.same_ids = earliest(&|candidate| candidate.resources == snapshot.resources);
        if !destination.covered.is_empty() {
            destinations.push(destination);
        }
    }

    let elsewhere: BTreeSet<&str> = destinations
        .iter()
        .flat_map(|destination| destination.covered.iter().copied())
        .collect();
    // A key that is already deleted stays DELETED, however much of it lives on elsewhere.
    if view.deleted || elsewhere.len() < of {
        return (fallback(elsewhere.len(), of), of, Vec::new());
    }

    let identical: Vec<MatchedVersion> = destinations
        .iter()
        .filter_map(|destination| destination.identical.clone())
        .collect();
    let same_ids: Vec<MatchedVersion> = destinations
        .iter()
        .filter_map(|destination| destination.same_ids.clone())
        .collect();
    let mut matches = Vec::new();
    let kind = if !identical.is_empty() {
        matches = identical;
        ObsoleteKind::MovedIdentical
    } else if !same_ids.is_empty() {
        matches = same_ids;
        ObsoleteKind::MovedSameIds
    } else if let Some(destination) = destinations
        .iter()
        .find(|destination| destination.covered.len() == of)
    {
        ObsoleteKind::Merged {
            into: views[destination.key].history.key.clone(),
        }
    } else {
        // Greedy cover: the key holding most of the remaining resources goes first.
        let mut remaining: BTreeSet<&str> = snapshot.resources.iter().map(String::as_str).collect();
        let mut into = 0;
        while !remaining.is_empty() {
            let best = destinations
                .iter()
                .max_by_key(|destination| {
                    (
                        destination.covered.intersection(&remaining).count(),
                        Reverse(destination.key),
                    )
                })
                .expect("a destination covers every resource");
            if best.covered.is_disjoint(&remaining) {
                break;
            }
            for resource in &best.covered {
                remaining.remove(resource);
            }
            into += 1;
        }
        ObsoleteKind::Split { into }
    };
    // Written again after its resources had shown up elsewhere: superseded, but not untouched.
    let moved = destinations
        .iter()
        .filter_map(|destination| destination.first)
        .min();
    if newer(view.latest, moved) {
        let kind = DeprecatedKind::WrittenAfterMove { kind };
        return (KeyStatus::Deprecated { kind }, of, matches);
    }
    (KeyStatus::Obsolete { kind }, of, matches)
}

/// Builds the branch report from the per-key histories and their per-version classification.
/// `bucket` and `errors` are left for the caller to fill in.
pub fn build(histories: &[KeyHistory], mut classified: Vec<KeyDuplicates>) -> BranchReport {
    sort_newest_first(&mut classified);
    let mut ordered: Vec<&KeyHistory> = histories.iter().collect();
    ordered.sort_by(|left, right| left.key.cmp(&right.key));
    let views: Vec<KeyView> = ordered.into_iter().map(view).collect();
    let edges = build_edges(&views);

    let mut versions: HashMap<String, Vec<ClassifiedVersion>> = classified
        .into_iter()
        .map(|key| (key.key, key.versions))
        .collect();
    let mut keys: Vec<(Option<Moment>, KeyNode)> = views
        .iter()
        .enumerate()
        .map(|(index, view)| {
            let (status, resources, matches) = judge(&views, index);
            let newest = view
                .history
                .versions
                .iter()
                .filter_map(|version| parsed_time(version.last_modified.as_deref()))
                .max();
            let head = status == KeyStatus::Active
                && view.snapshot.is_some_and(|snapshot| {
                    view.readable[snapshot].position == latest_index(&view.history.versions)
                });
            let node = KeyNode {
                key: view.history.key.clone(),
                detail: status.detail(),
                status,
                last_modified: view
                    .history
                    .versions
                    .get(latest_index(&view.history.versions))
                    .and_then(|version| version.last_modified.clone()),
                resources,
                version_id: view
                    .snapshot
                    .map(|snapshot| view.readable[snapshot].version.version_id.clone()),
                head,
                resource_hash: view.snapshot.and_then(|snapshot| {
                    match &view.readable[snapshot].version.content {
                        Some(Ok((_, fingerprint))) => resource_hash(fingerprint),
                        _ => None,
                    }
                }),
                matches,
                versions: versions.remove(&view.history.key).unwrap_or_default(),
            };
            (newest, node)
        })
        .collect();
    keys.sort_by_key(|(newest, _)| Reverse(*newest));
    BranchReport {
        bucket: String::new(),
        keys: keys.into_iter().map(|(_, node)| node).collect(),
        edges,
        errors: Vec::new(),
    }
}

/// Plain-words reason shown after a status in the terminal table.
fn reason(status: &KeyStatus) -> Option<String> {
    match status {
        KeyStatus::Active | KeyStatus::Deleted { of: 0, .. } => None,
        KeyStatus::Empty => Some("no resources left".to_string()),
        KeyStatus::Obsolete { kind } => Some(kind.reason()),
        KeyStatus::Deprecated {
            kind: DeprecatedKind::CopyOf { original },
        } => Some(format!("identical copy of the older {original}")),
        KeyStatus::Deprecated {
            kind: DeprecatedKind::WrittenAfterMove { kind },
        } => Some(format!("{}, but written again afterwards", kind.reason())),
        KeyStatus::Deleted { elsewhere, of } => {
            Some(format!("{elsewhere}/{of} resources exist elsewhere"))
        }
    }
}

/// Table order: the state files to keep first, then the removal candidates.
fn rank(status: &KeyStatus) -> u8 {
    match status {
        KeyStatus::Active => 0,
        KeyStatus::Deprecated { .. } => 1,
        KeyStatus::Obsolete { .. } => 2,
        KeyStatus::Empty => 3,
        KeyStatus::Deleted { .. } => 4,
    }
}

/// A key without `.tfstate` and a trailing `/terraform`.
fn stack_path(key: &str) -> &str {
    let trimmed = key.strip_suffix(".tfstate").unwrap_or(key);
    trimmed.strip_suffix("/terraform").unwrap_or(trimmed)
}

/// The stack name a group is titled with: the last component of the stack path.
fn stack_name(key: &str) -> &str {
    let path = stack_path(key);
    path.rsplit('/').next().unwrap_or(path)
}

/// Short keys for the history, keeping as few trailing path components as tell every key of
/// the group apart; falls back to the full keys.
fn short_keys<'a>(keys: &[&'a str]) -> HashMap<&'a str, String> {
    let deepest = keys
        .iter()
        .map(|key| key.split('/').count())
        .max()
        .unwrap_or(0);
    for depth in 2..=deepest.max(2) {
        let short: HashMap<&str, String> = keys
            .iter()
            .map(|&key| {
                let parts: Vec<&str> = stack_path(key).split('/').collect();
                let short = if parts.len() <= depth {
                    parts.join("/")
                } else {
                    format!("…/{}", parts[parts.len() - depth..].join("/"))
                };
                (key, short)
            })
            .collect();
        if short.values().collect::<HashSet<_>>().len() == keys.len() {
            return short;
        }
    }
    keys.iter().map(|&key| (key, key.to_string())).collect()
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn width<'s>(cells: impl IntoIterator<Item = &'s String>) -> usize {
    cells
        .into_iter()
        .map(|cell| cell.chars().count())
        .max()
        .unwrap_or(0)
}

struct Renderer<'a> {
    report: &'a BranchReport,
    verbose: bool,
    show_datetime: bool,
    full_hash: bool,
    now: chrono::DateTime<chrono::Utc>,
}

impl Renderer<'_> {
    fn when(&self, timestamp: Option<&str>) -> String {
        match timestamp {
            Some(timestamp) if self.show_datetime => {
                crate::formatter::terminal::sanitize_terminal(timestamp)
            }
            Some(timestamp) => crate::formatter::terminal::format_state_age(timestamp, self.now),
            None => String::new(),
        }
    }

    /// `@{version}`, followed by `HEAD` for the tip of a history.
    fn version(&self, version_id: &str, head: bool) -> String {
        let version = format!(
            "@{}",
            crate::formatter::terminal::sanitize_terminal(version_id)
        );
        if head {
            format!("{version} HEAD")
        } else {
            version
        }
    }

    /// LastModified of a version of a key in the report.
    fn last_modified<'n>(node: &'n KeyNode, version_id: &str) -> Option<&'n str> {
        node.versions
            .iter()
            .find(|version| version.version_id == version_id)
            .and_then(|version| version.last_modified.as_deref())
    }

    /// `key @version  when` of a matching version, followed by `→ @head HEAD  when` when the
    /// match is an older version of a key that has a HEAD.
    fn matched(&self, found: &MatchedVersion) -> String {
        let key = crate::formatter::terminal::sanitize_terminal(&found.key);
        let Some(node) = self.report.keys.iter().find(|node| node.key == found.key) else {
            return format!("{key} {}", self.version(&found.version_id, false));
        };
        let head = node.head.then_some(node.version_id.as_deref()).flatten();
        let mut line = format!(
            "{key} {}  {}",
            self.version(&found.version_id, head == Some(found.version_id.as_str())),
            self.when(Self::last_modified(node, &found.version_id))
        );
        if let Some(head) = head.filter(|&head| head != found.version_id) {
            line.push_str(&format!(
                "  → {}  {}",
                self.version(head, true),
                self.when(Self::last_modified(node, head))
            ));
        }
        line
    }

    /// `{title}  (N state files, K to keep)`.
    fn title(title: &str, members: &[&KeyNode]) -> String {
        let keep = members
            .iter()
            .filter(|node| node.status == KeyStatus::Active)
            .count();
        format!(
            "{}  ({}, {keep} to keep)",
            crate::formatter::terminal::sanitize_terminal(title),
            plural(members.len(), "state file")
        )
    }

    /// One row per state file: status, key, resource count, hash, last write, and reason.
    fn table(&self, members: &[&KeyNode], lines: &mut Vec<String>) {
        let rows: Vec<[String; 6]> = members
            .iter()
            .map(|node| {
                [
                    node.status.label().to_string(),
                    crate::formatter::terminal::sanitize_terminal(&node.key),
                    node.version_id
                        .as_deref()
                        .map(|version| self.version(version, node.head))
                        .unwrap_or_default(),
                    format!("{} res", node.resources),
                    node.resource_hash
                        .as_deref()
                        .map(|hash| format!("#{}", display_hash(hash, self.full_hash)))
                        .unwrap_or_default(),
                    self.when(node.last_modified.as_deref()),
                ]
            })
            .collect();
        let widths: Vec<usize> = (0..6)
            .map(|column| width(rows.iter().map(|row| &row[column])))
            .collect();
        for (node, row) in members.iter().zip(&rows) {
            let mut cells = vec![
                format!("{:<w$}", row[0], w = widths[0]),
                format!("{:<w$}", row[1], w = widths[1]),
                format!("{:<w$}", row[2], w = widths[2]),
                format!("{:>w$}", row[3], w = widths[3]),
            ];
            // A group without any hash gets no empty hash column.
            if widths[4] > 0 {
                cells.push(format!("{:<w$}", row[4], w = widths[4]));
            }
            cells.push(format!("{:<w$}", row[5], w = widths[5]));
            if let Some(reason) = reason(&node.status) {
                cells.push(crate::formatter::terminal::sanitize_terminal(&reason));
            }
            lines.push(format!("  {}", cells.join("  ")).trim_end().to_string());
            // Where the identical copy or the same resources are, below the key column.
            for found in &node.matches {
                lines.push(
                    format!("  {:w$}  ↳ {}", "", self.matched(found), w = widths[0])
                        .trim_end()
                        .to_string(),
                );
            }
            if self.verbose {
                for version in &node.versions {
                    lines.push(format!(
                        "      {}",
                        format_line(
                            &self.report.bucket,
                            &node.key,
                            version,
                            self.show_datetime,
                            self.full_hash,
                            self.now
                        )
                    ));
                }
            }
        }
    }

    /// The moves between the group's state files, oldest first.
    fn history(&self, members: &[&KeyNode], lines: &mut Vec<String>) {
        let keys: Vec<&str> = members.iter().map(|node| node.key.as_str()).collect();
        let mut edges: Vec<&Edge> = self
            .report
            .edges
            .iter()
            .filter(|edge| keys.contains(&edge.from.key.as_str()))
            .collect();
        edges.sort_by(|left, right| {
            parsed_time(left.at.as_deref())
                .cmp(&parsed_time(right.at.as_deref()))
                .then_with(|| left.from.key.cmp(&right.from.key))
                .then_with(|| left.to.key.cmp(&right.to.key))
        });
        let short = short_keys(&keys);
        let short = |key: &str| {
            crate::formatter::terminal::sanitize_terminal(
                short.get(key).map_or(key, String::as_str),
            )
        };
        let rows: Vec<[String; 4]> = edges
            .iter()
            .map(|edge| {
                [
                    self.when(edge.at.as_deref()),
                    short(&edge.from.key),
                    short(&edge.to.key),
                    plural(edge.resources, "resource"),
                ]
            })
            .collect();
        let widths: Vec<usize> = (0..4)
            .map(|column| width(rows.iter().map(|row| &row[column])))
            .collect();
        lines.push("  history".to_string());
        for (edge, row) in edges.iter().zip(&rows) {
            let mut line = format!(
                "    {:<w0$}  {:<w1$}  →  {:<w2$}  {:<w3$}  (source had {})",
                row[0],
                row[1],
                row[2],
                row[3],
                edge.of,
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
                w3 = widths[3],
            );
            if self.verbose {
                line.push_str(&format!(
                    "  @{} → @{}",
                    crate::formatter::terminal::sanitize_terminal(&edge.from.version_id),
                    crate::formatter::terminal::sanitize_terminal(&edge.to.version_id)
                ));
            }
            lines.push(line);
        }
    }
}

/// Sorts a group into table order: ACTIVE, DEPRECATED, OBSOLETE, EMPTY, DELETED, each newest
/// first.
fn table_order(members: &mut [&KeyNode]) {
    members.sort_by(|left, right| {
        rank(&left.status)
            .cmp(&rank(&right.status))
            .then_with(|| {
                parsed_time(right.last_modified.as_deref())
                    .cmp(&parsed_time(left.last_modified.as_deref()))
            })
            .then_with(|| left.key.cmp(&right.key))
    });
}

/// Renders one group per family of state files linked by moves, each as a status table and a
/// history of the moves, then the state files without any moves, then the removal candidates.
pub fn format_terminal(
    report: &BranchReport,
    verbose: bool,
    show_datetime: bool,
    full_hash: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    if report.keys.is_empty() {
        return String::new();
    }
    let renderer = Renderer {
        report,
        verbose,
        show_datetime,
        full_hash,
        now,
    };
    let nodes: HashMap<&str, &KeyNode> = report
        .keys
        .iter()
        .map(|node| (node.key.as_str(), node))
        .collect();
    let mut neighbours: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &report.edges {
        neighbours
            .entry(&edge.from.key)
            .or_default()
            .push(&edge.to.key);
        neighbours
            .entry(&edge.to.key)
            .or_default()
            .push(&edge.from.key);
    }

    // `report.keys` is sorted newest first, so groups come out ordered by their newest file.
    let mut grouped: HashSet<&str> = HashSet::new();
    let mut groups: Vec<Vec<&KeyNode>> = Vec::new();
    let mut standalone: Vec<&KeyNode> = Vec::new();
    for node in &report.keys {
        if grouped.contains(node.key.as_str()) {
            continue;
        }
        if !neighbours.contains_key(node.key.as_str()) {
            standalone.push(node);
            continue;
        }
        grouped.insert(&node.key);
        let mut queue = vec![node.key.as_str()];
        let mut members = Vec::new();
        while let Some(key) = queue.pop() {
            members.extend(nodes.get(key));
            for &next in &neighbours[key] {
                if grouped.insert(next) {
                    queue.push(next);
                }
            }
        }
        groups.push(members);
    }

    let mut blocks: Vec<Vec<String>> = Vec::new();
    for mut members in groups {
        // Titled after the state file to keep, or the newest one when none is left.
        let newest = members
            .iter()
            .max_by_key(|node| parsed_time(node.last_modified.as_deref()))
            .map(|node| node.key.clone())
            .unwrap_or_default();
        table_order(&mut members);
        let title = members
            .iter()
            .find(|node| node.status == KeyStatus::Active)
            .map_or(newest.as_str(), |node| node.key.as_str());
        let mut lines = vec![Renderer::title(stack_name(title), &members), String::new()];
        renderer.table(&members, &mut lines);
        lines.push(String::new());
        renderer.history(&members, &mut lines);
        blocks.push(lines);
    }
    if !standalone.is_empty() {
        table_order(&mut standalone);
        let mut lines = vec![Renderer::title("standalone", &standalone), String::new()];
        renderer.table(&standalone, &mut lines);
        blocks.push(lines);
    }

    let counts: Vec<(usize, &str)> = ["DEPRECATED", "OBSOLETE", "EMPTY"]
        .into_iter()
        .map(|label| {
            let count = report
                .keys
                .iter()
                .filter(|node| node.status.label() == label)
                .count();
            (count, label)
        })
        .filter(|(count, _)| *count > 0)
        .collect();
    let total: usize = counts.iter().map(|(count, _)| count).sum();
    let parts: Vec<String> = counts
        .iter()
        .map(|(count, label)| format!("{count} {label}"))
        .collect();
    blocks.push(vec![if parts.is_empty() {
        "removal candidates: none".to_string()
    } else {
        format!("removal candidates: {total} ({})", parts.join(", "))
    }]);

    let mut output = String::new();
    for (position, lines) in blocks.iter().enumerate() {
        if position > 0 {
            output.push('\n');
        }
        for line in lines {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

pub fn format_json(report: &BranchReport) -> String {
    serde_json::to_string_pretty(report).expect("branch report serializes")
}
