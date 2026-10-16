use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    pub window_start: String,
    pub window_end: String,
    pub regions: Vec<String>,
    pub show_atomic_authors: bool,
    pub status: AuditStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<String>,
    #[serde(default)]
    pub observations: Vec<AuditObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditObservation {
    pub event_id: String,
    pub event_time: String,
    pub event_name: String,
    pub event_source: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub principal_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub principal_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub principal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub email_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub identity_hint_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_hint: Option<String>,
    pub match_basis: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Pending,
    Complete,
    NoMatch,
    UnsupportedResource,
    MissingIdentity,
    OutsideRetention,
    PartialRetention,
    CrossAccountUnavailable,
    Unavailable,
    Incomplete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerIdentity {
    pub account_id: String,
    pub arn: String,
    pub user_id: String,
}

/// A normalized CloudTrail management event. Only fields needed for matching and evidence are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedEvent {
    pub event_id: String,
    pub event_time: DateTime<Utc>,
    pub event_name: String,
    pub event_source: String,
    pub event_category: Option<String>,
    pub account_id: Option<String>,
    pub region: Option<String>,
    pub read_only: Option<bool>,
    pub error_code: Option<String>,
    pub resources: Vec<NormalizedResource>,
    pub request_parameters: Option<serde_json::Value>,
    pub response_elements: Option<serde_json::Value>,
    pub observation: AuditObservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedResource {
    pub resource_type: Option<String>,
    pub resource_name: Option<String>,
}

/// Match one normalized CloudTrail event to a changed managed resource using an exact,
/// service-specific identity. Unsupported resources and ambiguous/sensitive IDs fail closed.
pub fn match_resource_event(
    resource: &crate::Resource,
    event: &NormalizedEvent,
    account_id: &str,
    region: &str,
) -> Option<AuditObservation> {
    if event.account_id.as_deref() != Some(account_id)
        || event.region.as_deref() != Some(region)
        || event.event_category.as_deref() != Some("Management")
        || event.read_only != Some(false)
        || event.error_code.is_some()
    {
        return None;
    }

    if resource.type_name == "aws_iam_role_policy_attachment" {
        let basis = match_iam_role_policy_attachment(resource, event)?;
        let mut observation = event.observation.clone();
        observation.match_basis = basis;
        return Some(observation);
    }

    if resource.type_name != "aws_vpc_endpoint" || event.event_source != "ec2.amazonaws.com" {
        return None;
    }

    let identities = vpc_endpoint_identities(resource);
    if identities.before.is_none() && identities.after.is_none() {
        return None;
    }
    let operation = match (&resource.action, event.event_name.as_str()) {
        (crate::ActionType::Create, "CreateVpcEndpoint") => EndpointOperation::Create,
        (crate::ActionType::Update, "ModifyVpcEndpoint") => EndpointOperation::Modify,
        (crate::ActionType::Destroy, "DeleteVpcEndpoints") => EndpointOperation::Delete,
        (crate::ActionType::Update, "CreateVpcEndpoint") => EndpointOperation::Create,
        (crate::ActionType::Update, "DeleteVpcEndpoints") => EndpointOperation::Delete,
        _ => return None,
    };
    let changed_identity = identities
        .before
        .as_ref()
        .zip(identities.after.as_ref())
        .is_some_and(|(before, after)| before != after);
    if matches!(
        operation,
        EndpointOperation::Create | EndpointOperation::Delete
    ) && resource.action == crate::ActionType::Update
        && !changed_identity
    {
        return None;
    }

    let event_ids = vpc_endpoint_event_ids(event, operation);
    let identity = match operation {
        EndpointOperation::Create => identities.after.as_ref(),
        EndpointOperation::Delete => identities.before.as_ref(),
        EndpointOperation::Modify => identities.after.as_ref().or(identities.before.as_ref()),
    }?;
    let basis = event_ids
        .into_iter()
        .find_map(|(event_id, basis)| (event_id == *identity).then_some(basis))?;

    let mut observation = event.observation.clone();
    observation.match_basis = basis;
    Some(observation)
}

#[derive(Clone, Copy)]
enum EndpointOperation {
    Create,
    Modify,
    Delete,
}

#[derive(Default)]
struct EndpointIdentities {
    before: Option<String>,
    after: Option<String>,
}

fn vpc_endpoint_identities(resource: &crate::Resource) -> EndpointIdentities {
    let mut identities = EndpointIdentities::default();
    // If either side marks the ID sensitive, do not use any ID value from this resource.
    if resource
        .changes
        .iter()
        .any(|change| change.path == ["id"] && change.sensitive)
    {
        return identities;
    }

    let attributes_id = resource
        .attributes
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| is_vpc_endpoint_id(value));
    match resource.action {
        crate::ActionType::Create => identities.after = attributes_id.map(str::to_string),
        crate::ActionType::Destroy => identities.before = attributes_id.map(str::to_string),
        crate::ActionType::Update => identities.after = attributes_id.map(str::to_string),
        _ => return identities,
    }

    for change in resource
        .changes
        .iter()
        .filter(|change| change.path == ["id"])
    {
        if change.sensitive {
            continue;
        }
        if let Some(value) = change
            .before
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .filter(|value| is_vpc_endpoint_id(value))
        {
            identities.before = Some(value.to_string());
        }
        if let Some(value) = change
            .after
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .filter(|value| is_vpc_endpoint_id(value))
        {
            identities.after = Some(value.to_string());
        }
    }
    identities
}

fn is_vpc_endpoint_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("vpce-") else {
        return false;
    };
    !suffix.is_empty() && suffix.len() <= 64 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn vpc_endpoint_event_ids(
    event: &NormalizedEvent,
    operation: EndpointOperation,
) -> Vec<(String, String)> {
    let mut identities = Vec::new();
    for resource in &event.resources {
        if resource.resource_type.as_deref() == Some("AWS::EC2::VpcEndpoint") {
            if let Some(name) = resource
                .resource_name
                .as_deref()
                .filter(|name| is_vpc_endpoint_id(name))
            {
                identities.push((
                    name.to_string(),
                    "CloudTrail resource VPC endpoint ID".into(),
                ));
            }
        }
    }
    match operation {
        EndpointOperation::Create => {
            if let Some(id) = event
                .response_elements
                .as_ref()
                .and_then(|value| value.get("vpcEndpoint"))
                .and_then(|value| value.get("vpcEndpointId"))
                .and_then(serde_json::Value::as_str)
                .filter(|value| is_vpc_endpoint_id(value))
            {
                identities.push((
                    id.to_string(),
                    "responseElements.vpcEndpoint.vpcEndpointId".into(),
                ));
            }
        }
        EndpointOperation::Modify => {
            let succeeded = event
                .response_elements
                .as_ref()
                .and_then(|value| value.get("return"))
                .and_then(serde_json::Value::as_bool)
                != Some(false);
            if succeeded {
                if let Some(id) = event
                    .request_parameters
                    .as_ref()
                    .and_then(|value| value.get("vpcEndpointId"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| is_vpc_endpoint_id(value))
                {
                    identities.push((id.to_string(), "requestParameters.vpcEndpointId".into()));
                }
            }
        }
        EndpointOperation::Delete => {
            let unsuccessful_ids: std::collections::HashSet<&str> = event
                .response_elements
                .as_ref()
                .and_then(|value| value.get("unsuccessful"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    entry
                        .get("resourceId")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| is_vpc_endpoint_id(id))
                })
                .collect();
            if let Some(ids) = event
                .request_parameters
                .as_ref()
                .and_then(|value| value.get("vpcEndpointIds"))
                .and_then(serde_json::Value::as_array)
            {
                identities.extend(ids.iter().filter_map(|value| {
                    value
                        .as_str()
                        .filter(|id| is_vpc_endpoint_id(id) && !unsuccessful_ids.contains(id))
                        .map(|id| (id.to_string(), "requestParameters.vpcEndpointIds".into()))
                }));
            }
        }
    }
    identities
}

fn lookup_candidate(resource: &crate::Resource) -> bool {
    is_mutating_action(&resource.action)
        && match resource.type_name.as_str() {
            "aws_vpc_endpoint" => {
                let identities = vpc_endpoint_identities(resource);
                identities.before.is_some() || identities.after.is_some()
            }
            "aws_iam_role_policy_attachment" => iam_attachment_identity(resource).is_some(),
            _ => false,
        }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IamAttachmentIdentity {
    role: String,
    policy_arn: String,
}

fn iam_attachment_identity(resource: &crate::Resource) -> Option<IamAttachmentIdentity> {
    let side = match resource.action {
        crate::ActionType::Create => IdentitySide::After,
        crate::ActionType::Destroy => IdentitySide::Before,
        _ => return None,
    };
    let policy_arn = iam_identity_field(resource, "policy_arn", side)?;
    safe_arn(&policy_arn)?;
    Some(IamAttachmentIdentity {
        role: iam_identity_field(resource, "role", side)?,
        policy_arn,
    })
}

#[derive(Clone, Copy)]
enum IdentitySide {
    Before,
    After,
}

fn iam_identity_field(
    resource: &crate::Resource,
    name: &str,
    side: IdentitySide,
) -> Option<String> {
    if resource
        .changes
        .iter()
        .any(|change| change.sensitive && change.path == [name])
    {
        return None;
    }
    let value = match side {
        IdentitySide::After => resource
            .attributes
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        IdentitySide::Before => None,
    }
    .or_else(|| {
        resource
            .changes
            .iter()
            .filter(|change| change.path == [name])
            .find_map(|change| {
                let value = match side {
                    IdentitySide::Before => change.before.as_ref(),
                    IdentitySide::After => change.after.as_ref(),
                };
                value
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
    })?;
    safe_identity_field(&value).map(str::to_string)
}

fn match_iam_role_policy_attachment(
    resource: &crate::Resource,
    event: &NormalizedEvent,
) -> Option<String> {
    if event.event_source != "iam.amazonaws.com" {
        return None;
    }
    let identity = iam_attachment_identity(resource)?;
    let expected_event = match resource.action {
        crate::ActionType::Create => "AttachRolePolicy",
        crate::ActionType::Destroy => "DetachRolePolicy",
        _ => return None,
    };
    if event.event_name != expected_event {
        return None;
    }
    let parameters = event.request_parameters.as_ref()?;
    let role = parameters
        .get("roleName")
        .and_then(serde_json::Value::as_str)?;
    let policy_arn = parameters
        .get("policyArn")
        .and_then(serde_json::Value::as_str)?;
    (role == identity.role && policy_arn == identity.policy_arn)
        .then(|| "requestParameters.roleName and requestParameters.policyArn".to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupRequest {
    pub region: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub next_token: Option<String>,
    pub page_size: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupPage {
    pub events: Vec<NormalizedEvent>,
    pub next_token: Option<String>,
    pub malformed_events: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupOutcome {
    pub events: Vec<NormalizedEvent>,
    pub status: AuditStatus,
    pub message: Option<String>,
    pub queried_start: DateTime<Utc>,
    pub queried_end: DateTime<Utc>,
    pub pages: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct AuditTransportError {
    pub message: String,
    pub access_denied: bool,
}

#[async_trait]
pub trait AuditTransport: Send + Sync {
    async fn caller_identity(&self) -> std::result::Result<CallerIdentity, AuditTransportError>;

    async fn lookup_events(
        &self,
        request: LookupRequest,
    ) -> std::result::Result<LookupPage, AuditTransportError>;
}

/// AWS-backed CloudTrail Event History transport using the standard SDK credential chain.
#[derive(Clone)]
pub struct AwsAuditTransport {
    sts: aws_sdk_sts::Client,
    sdk_config: aws_config::SdkConfig,
}

impl AwsAuditTransport {
    pub async fn from_default_config() -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Self::from_sdk_config(&config)
    }

    /// Build clients from the shared AWS configuration, retaining injectable endpoints for tests.
    pub fn from_sdk_config(config: &aws_config::SdkConfig) -> Self {
        let mut sts_config = aws_sdk_sts::config::Builder::from(config);
        if let Some(endpoint_url) = config.endpoint_url() {
            sts_config = sts_config.endpoint_url(endpoint_url);
        }
        if let Some(retry_config) = config.retry_config() {
            sts_config = sts_config.retry_config(retry_config.clone());
        }
        Self {
            sts: aws_sdk_sts::Client::from_conf(sts_config.build()),
            sdk_config: config.clone(),
        }
    }

    fn cloudtrail_for_region(&self, region: &str) -> aws_sdk_cloudtrail::Client {
        let mut builder = aws_sdk_cloudtrail::config::Builder::from(&self.sdk_config)
            .region(aws_sdk_cloudtrail::config::Region::new(region.to_string()));
        if let Some(endpoint_url) = self.sdk_config.endpoint_url() {
            builder = builder.endpoint_url(endpoint_url);
        }
        if let Some(retry_config) = self.sdk_config.retry_config() {
            builder = builder.retry_config(retry_config.clone());
        }
        aws_sdk_cloudtrail::Client::from_conf(builder.build())
    }
}

#[async_trait]
impl AuditTransport for AwsAuditTransport {
    async fn caller_identity(&self) -> std::result::Result<CallerIdentity, AuditTransportError> {
        let identity = self
            .sts
            .get_caller_identity()
            .send()
            .await
            .map_err(|error| sdk_transport_error(error.to_string()))?;
        let account_id = identity
            .account()
            .filter(|value| valid_account_id(value))
            .ok_or_else(|| transport_error("STS returned no valid account ID", false))?;
        let arn = identity
            .arn()
            .filter(|value| safe_arn(value).is_some())
            .ok_or_else(|| transport_error("STS returned no valid caller ARN", false))?;
        let user_id = identity
            .user_id()
            .filter(|value| safe_identity_field(value).is_some())
            .ok_or_else(|| transport_error("STS returned no valid caller user ID", false))?;
        Ok(CallerIdentity {
            account_id: account_id.to_string(),
            arn: arn.to_string(),
            user_id: user_id.to_string(),
        })
    }

    async fn lookup_events(
        &self,
        request: LookupRequest,
    ) -> std::result::Result<LookupPage, AuditTransportError> {
        if !safe_region(&request.region)
            || request.start_time > request.end_time
            || !(1..=MAX_CLOUDTRAIL_PAGE_SIZE).contains(&request.page_size)
        {
            return Err(transport_error(
                "invalid CloudTrail lookup region or time interval",
                false,
            ));
        }
        if request.next_token.as_deref().is_some_and(|token| {
            token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control)
        }) {
            return Err(transport_error(
                "invalid CloudTrail continuation token",
                false,
            ));
        }
        let lookup_attribute = aws_sdk_cloudtrail::types::LookupAttribute::builder()
            .attribute_key(aws_sdk_cloudtrail::types::LookupAttributeKey::ReadOnly)
            .attribute_value("false")
            .build()
            .map_err(|error| {
                transport_error(format!("could not build CloudTrail lookup: {error}"), false)
            })?;
        let mut lookup = self
            .cloudtrail_for_region(&request.region)
            .lookup_events()
            .lookup_attributes(lookup_attribute)
            .start_time(to_sdk_datetime(request.start_time))
            .end_time(to_sdk_datetime(request.end_time))
            .max_results(
                i32::try_from(request.page_size).unwrap_or(MAX_CLOUDTRAIL_PAGE_SIZE as i32),
            );
        if let Some(token) = request.next_token {
            lookup = lookup.next_token(token);
        }
        let output = lookup
            .send()
            .await
            .map_err(|error| sdk_transport_error(error.to_string()))?;
        let mut malformed_events = 0;
        let events = output
            .events()
            .iter()
            .filter_map(|event| match normalize_cloudtrail_event(event) {
                Some(event) => Some(event),
                None => {
                    malformed_events += 1;
                    None
                }
            })
            .collect();
        Ok(LookupPage {
            events,
            next_token: output
                .next_token()
                .filter(|token| !token.is_empty())
                .map(str::to_string),
            malformed_events,
        })
    }
}

/// Query event history with bounded pagination and independent evidence validation.
pub async fn query_event_history(
    transport: &dyn AuditTransport,
    caller: &CallerIdentity,
    region: &str,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    page_size: u32,
    now: DateTime<Utc>,
) -> LookupOutcome {
    let retention_start =
        now - chrono::Duration::from_std(CLOUDTRAIL_RETENTION).expect("90 days fit");
    if end_time <= retention_start {
        return lookup_outcome(
            Vec::new(),
            AuditStatus::OutsideRetention,
            Some("lookup interval is older than CloudTrail Event History retention".to_string()),
            start_time,
            end_time,
            0,
        );
    }
    if !safe_region(region) || start_time > end_time || !valid_account_id(&caller.account_id) {
        return lookup_outcome(
            Vec::new(),
            AuditStatus::Failed,
            Some("invalid caller account, region, or CloudTrail lookup interval".to_string()),
            start_time,
            end_time,
            0,
        );
    }
    let queried_start = start_time.max(retention_start);
    let mut events = Vec::new();
    let mut malformed = false;
    let mut next_token = None;
    let mut seen_tokens = std::collections::HashSet::new();
    let mut pages = 0;
    let mut incomplete_message = None;

    loop {
        if pages == MAX_LOOKUP_PAGES {
            incomplete_message = Some("CloudTrail pagination limit reached".to_string());
            break;
        }
        let request = LookupRequest {
            region: region.to_string(),
            start_time: queried_start,
            end_time,
            next_token: next_token.clone(),
            page_size,
        };
        let page = match transport.lookup_events(request).await {
            Ok(page) => page,
            Err(error) => {
                let message = if error.access_denied {
                    "CloudTrail access denied".to_string()
                } else {
                    format!(
                        "CloudTrail lookup failed: {}",
                        sanitize_diagnostic(&error.message)
                    )
                };
                incomplete_message = Some(message);
                break;
            }
        };
        pages += 1;
        malformed |= page.malformed_events > 0;
        events.extend(page.events.into_iter().filter(|event| {
            valid_management_mutation(event, caller, region, queried_start, end_time)
        }));
        let Some(token) = page.next_token else {
            break;
        };
        if !seen_tokens.insert(token.clone()) {
            incomplete_message =
                Some("CloudTrail returned a repeated continuation token".to_string());
            break;
        }
        next_token = Some(token);
    }

    if malformed && incomplete_message.is_none() {
        incomplete_message =
            Some("CloudTrail returned malformed events that were ignored".to_string());
    }
    let status = if incomplete_message.is_some() {
        AuditStatus::Incomplete
    } else if start_time < retention_start {
        AuditStatus::PartialRetention
    } else if events.is_empty() {
        AuditStatus::NoMatch
    } else {
        AuditStatus::Complete
    };
    lookup_outcome(
        events,
        status,
        incomplete_message,
        queried_start,
        end_time,
        pages,
    )
}

fn valid_management_mutation(
    event: &NormalizedEvent,
    caller: &CallerIdentity,
    region: &str,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) -> bool {
    event.event_time >= start_time
        && event.event_time <= end_time
        && event.event_category.as_deref() == Some("Management")
        && event.read_only == Some(false)
        && event.error_code.is_none()
        && event.account_id.as_deref() == Some(caller.account_id.as_str())
        && event.region.as_deref() == Some(region)
        && !event.event_id.is_empty()
        && supported_mutation_operation(&event.event_source, &event.event_name)
}

fn supported_mutation_operation(event_source: &str, event_name: &str) -> bool {
    // Keep the history coordinator limited to exact adapters until each service's
    // request/response identity paths have dedicated fixtures.
    matches!(
        (event_source, event_name),
        (
            "ec2.amazonaws.com",
            "CreateVpcEndpoint" | "ModifyVpcEndpoint" | "DeleteVpcEndpoints"
        ) | ("iam.amazonaws.com", "AttachRolePolicy" | "DetachRolePolicy")
    )
}

fn lookup_outcome(
    events: Vec<NormalizedEvent>,
    status: AuditStatus,
    message: Option<String>,
    queried_start: DateTime<Utc>,
    queried_end: DateTime<Utc>,
    pages: usize,
) -> LookupOutcome {
    LookupOutcome {
        events,
        status,
        message,
        queried_start,
        queried_end,
        pages,
    }
}

fn transport_error(message: impl Into<String>, access_denied: bool) -> AuditTransportError {
    AuditTransportError {
        message: message.into(),
        access_denied,
    }
}

fn sdk_transport_error(message: String) -> AuditTransportError {
    let lower = message.to_ascii_lowercase();
    let access_denied = lower.contains("accessdenied") || lower.contains("unauthorized");
    transport_error(message, access_denied)
}

fn sanitize_diagnostic(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect()
}

fn to_sdk_datetime(value: DateTime<Utc>) -> aws_smithy_types::DateTime {
    aws_smithy_types::DateTime::from_secs(value.timestamp())
}

/// The maximum supported look-back window for CloudTrail event history.
pub const MAX_AUTHOR_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_AUTHOR_WINDOW: Duration = Duration::from_secs(5 * 60);
pub const CLOUDTRAIL_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);
pub const RETENTION_NOTICE: &str = "tfdiff: CloudTrail event history retains regional management events for 90 days; older windows may be unavailable. Matched events are evidence of AWS actions, not proof of Terraform/OpenTofu causality.";
pub const MAX_LOOKUP_PAGES: usize = 1000;
pub const MAX_CLOUDTRAIL_PAGE_SIZE: u32 = 50;
pub const DEFAULT_CLOUDTRAIL_PAGE_SIZE: u32 = MAX_CLOUDTRAIL_PAGE_SIZE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorOptions {
    pub show_authors: bool,
    pub show_atomic_authors: bool,
    pub regions: Vec<String>,
    pub log_groups: Vec<String>,
    pub window: Duration,
    pub page_size: u32,
    pub iam_region: Option<String>,
    pub global_region: Option<String>,
    pub auxiliary_options_provided: bool,
}

impl AuthorOptions {
    pub fn enabled(&self) -> bool {
        self.show_authors || self.show_atomic_authors
    }

    /// Whether a filtered plan contains at least one safe, supported identity to look up.
    pub fn has_lookup_candidates(plan: &crate::TerraformPlan) -> bool {
        plan.resources.iter().any(lookup_candidate)
    }

    pub fn validate(&self) -> crate::Result<()> {
        if !self.enabled() && self.auxiliary_options_provided {
            return Err(argument_error(
                "--author-region, --author-log-group, --author-window, --author-page-size, --aws-iam-region, and --aws-global-region require --show-authors or --show-atomic-authors",
            ));
        }
        if self.regions.iter().any(|region| !safe_region(region)) {
            return Err(argument_error(
                "--author-region must be a valid AWS region identifier",
            ));
        }
        if self
            .iam_region
            .iter()
            .chain(self.global_region.iter())
            .any(|region| !safe_region(region))
        {
            return Err(argument_error(
                "--aws-iam-region and --aws-global-region must be valid AWS region identifiers",
            ));
        }
        if !(1..=MAX_CLOUDTRAIL_PAGE_SIZE).contains(&self.page_size) {
            return Err(argument_error(
                "--author-page-size must be between 1 and 50",
            ));
        }
        Ok(())
    }
}

/// Enrich the already-filtered state-diff plan with evidence from CloudTrail Event History.
/// A lookup failure is represented in the report and never changes comparison success.
pub async fn enrich_plan_with_transport(
    plan: &mut crate::TerraformPlan,
    options: &AuthorOptions,
    transport: Option<&dyn AuditTransport>,
    default_region: Option<&str>,
    now: DateTime<Utc>,
) -> AuditReport {
    let window = plan
        .source_versions
        .as_ref()
        .and_then(|versions| versions.right.as_ref())
        .and_then(|version| version.last_modified.as_deref())
        .and_then(|last_modified| audit_window(last_modified, options.window).ok());
    let (window_start, window_end) = window
        .map(|(start, end)| {
            (
                start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )
        })
        .unwrap_or_default();

    let mut configured_regions = Vec::new();
    if let Some(region) = default_region.filter(|region| safe_region(region)) {
        configured_regions.push(region.to_string());
    }
    for region in &options.regions {
        if !configured_regions.contains(region) {
            configured_regions.push(region.clone());
        }
    }

    let mut has_unsupported = false;
    let mut has_missing_identity = false;
    let mut eligible_indices = Vec::new();
    for (index, resource) in plan.resources.iter_mut().enumerate() {
        if !is_mutating_action(&resource.action) {
            resource.audit_attribution = None;
            continue;
        }
        if !supported_resource_type(resource) {
            has_unsupported = true;
            resource.audit_attribution = Some(crate::ResourceAuditAttribution {
                status: AuditStatus::UnsupportedResource,
                observations: Vec::new(),
            });
        } else if !lookup_candidate(resource) {
            has_missing_identity = true;
            resource.audit_attribution = Some(crate::ResourceAuditAttribution {
                status: AuditStatus::MissingIdentity,
                observations: Vec::new(),
            });
        } else {
            eligible_indices.push(index);
            resource.audit_attribution = Some(crate::ResourceAuditAttribution {
                status: AuditStatus::Pending,
                observations: Vec::new(),
            });
        }
    }

    let mut report = AuditReport {
        window_start,
        window_end,
        regions: configured_regions.clone(),
        show_atomic_authors: options.show_atomic_authors,
        status: AuditStatus::Pending,
        message: None,
        observations: Vec::new(),
    };

    if eligible_indices.is_empty() {
        report.status = if has_unsupported {
            AuditStatus::UnsupportedResource
        } else if has_missing_identity {
            AuditStatus::MissingIdentity
        } else {
            AuditStatus::NoMatch
        };
        report.message = (!options.log_groups.is_empty()).then(|| {
            "CloudWatch Logs supplementation was not performed; only CloudTrail Event History is enabled".to_string()
        });
        plan.author_attribution = Some(report.clone());
        return report;
    }

    let Some(last_modified) = plan
        .source_versions
        .as_ref()
        .and_then(|versions| versions.right.as_ref())
        .and_then(|version| version.last_modified.as_deref())
    else {
        mark_resource_status(plan, &eligible_indices, AuditStatus::Incomplete);
        report.status = AuditStatus::Incomplete;
        report.message = Some(
            "right-side S3 LastModified is unavailable; no audit window was inferred".to_string(),
        );
        plan.author_attribution = Some(report.clone());
        return report;
    };
    let (start_time, end_time) = match audit_window(last_modified, options.window) {
        Ok(window) => window,
        Err(_) => {
            mark_resource_status(plan, &eligible_indices, AuditStatus::Incomplete);
            report.status = AuditStatus::Incomplete;
            report.message = Some(
                "right-side S3 LastModified is invalid; no audit window was inferred".to_string(),
            );
            plan.author_attribution = Some(report.clone());
            return report;
        }
    };
    let retention_start =
        now - chrono::Duration::from_std(CLOUDTRAIL_RETENTION).expect("90 days fit");
    if end_time <= retention_start {
        mark_resource_status(plan, &eligible_indices, AuditStatus::OutsideRetention);
        report.status = AuditStatus::OutsideRetention;
        report.message = Some(
            "lookup interval is outside CloudTrail Event History retention; no AWS audit requests were made".to_string(),
        );
        plan.author_attribution = Some(report.clone());
        return report;
    }
    let any_global = eligible_indices
        .iter()
        .any(|&index| global_service_resource(&plan.resources[index]));
    if configured_regions.is_empty() && !any_global {
        mark_resource_status(plan, &eligible_indices, AuditStatus::Unavailable);
        report.status = AuditStatus::Unavailable;
        report.message = Some(
            "no configured AWS region is available; use --author-region to select a lookup region"
                .to_string(),
        );
        plan.author_attribution = Some(report.clone());
        return report;
    }
    let Some(transport) = transport else {
        mark_resource_status(plan, &eligible_indices, AuditStatus::Unavailable);
        report.status = AuditStatus::Unavailable;
        report.message = Some("AWS audit transport is unavailable".to_string());
        plan.author_attribution = Some(report.clone());
        return report;
    };
    if !options.log_groups.is_empty() {
        report.message = Some(
            "CloudWatch Logs supplementation was not performed; only CloudTrail Event History is enabled".to_string(),
        );
    }

    let caller = match transport.caller_identity().await {
        Ok(caller) if valid_account_id(&caller.account_id) => caller,
        Ok(_) | Err(_) => {
            mark_resource_status(plan, &eligible_indices, AuditStatus::Unavailable);
            report.status = AuditStatus::Unavailable;
            report.message = Some(
                "AWS caller identity could not be resolved; audit lookup was skipped".to_string(),
            );
            plan.author_attribution = Some(report.clone());
            return report;
        }
    };

    let mut lookup_cache: std::collections::HashMap<String, LookupOutcome> =
        std::collections::HashMap::new();
    for index in eligible_indices {
        let phases = lookup_phases(&plan.resources[index], options, &configured_regions);
        let mut observations = Vec::new();
        let mut resource_statuses = Vec::new();
        for phase in phases {
            for region in phase {
                if !lookup_cache.contains_key(&region) {
                    let outcome = match tokio::time::timeout(
                        Duration::from_secs(60),
                        query_event_history(
                            transport,
                            &caller,
                            &region,
                            start_time,
                            end_time,
                            options.page_size,
                            now,
                        ),
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(_) => LookupOutcome {
                            events: Vec::new(),
                            status: AuditStatus::Incomplete,
                            message: Some(
                                "CloudTrail lookup exceeded the 60-second query deadline"
                                    .to_string(),
                            ),
                            queried_start: start_time,
                            queried_end: end_time,
                            pages: 0,
                        },
                    };
                    if let Some(message) = outcome.message.as_deref() {
                        report.message = Some(match report.message.take() {
                            Some(previous) => format!("{previous}; {region}: {message}"),
                            None => format!("{region}: {message}"),
                        });
                    }
                    lookup_cache.insert(region.clone(), outcome);
                }
                let outcome = &lookup_cache[&region];
                resource_statuses.push(outcome.status.clone());
                for event in &outcome.events {
                    if let Some(observation) = match_resource_event(
                        &plan.resources[index],
                        event,
                        &caller.account_id,
                        &region,
                    ) {
                        observations.push(observation);
                    }
                }
            }
            if !observations.is_empty() {
                break;
            }
        }
        deduplicate_observations(&mut observations);
        report.observations.extend(observations.iter().cloned());
        let status = resource_status(&observations, &resource_statuses);
        plan.resources[index].audit_attribution = Some(crate::ResourceAuditAttribution {
            status,
            observations,
        });
    }
    let lookup_statuses: Vec<AuditStatus> = lookup_cache
        .values()
        .map(|outcome| outcome.status.clone())
        .collect();
    deduplicate_observations(&mut report.observations);
    report.status = report_status(
        &report.observations,
        &lookup_statuses,
        has_unsupported,
        has_missing_identity,
    );
    plan.author_attribution = Some(report.clone());
    report
}

fn is_mutating_action(action: &crate::ActionType) -> bool {
    matches!(
        action,
        crate::ActionType::Create | crate::ActionType::Update | crate::ActionType::Destroy
    )
}

const DEFAULT_GLOBAL_SERVICE_REGION: &str = "us-east-1";

fn supported_resource_type(resource: &crate::Resource) -> bool {
    matches!(
        resource.type_name.as_str(),
        "aws_vpc_endpoint" | "aws_iam_role_policy_attachment"
    )
}

// Global-service families whose CloudTrail management events are recorded in us-east-1.
// Only IAM has an active matcher; the others are reserved and unverified.
fn global_service_resource(resource: &crate::Resource) -> bool {
    [
        "aws_iam_",
        "aws_cloudfront_",
        "aws_route53_",
        "aws_organizations_",
    ]
    .iter()
    .any(|prefix| resource.type_name.starts_with(prefix))
}

fn global_lookup_region(resource: &crate::Resource, options: &AuthorOptions) -> Option<String> {
    if !global_service_resource(resource) {
        return None;
    }
    let override_region = if resource.type_name.starts_with("aws_iam_") {
        options.iam_region.as_deref()
    } else {
        options.global_region.as_deref()
    };
    Some(
        override_region
            .unwrap_or(DEFAULT_GLOBAL_SERVICE_REGION)
            .to_string(),
    )
}

fn lookup_phases(
    resource: &crate::Resource,
    options: &AuthorOptions,
    configured: &[String],
) -> Vec<Vec<String>> {
    match global_lookup_region(resource, options) {
        Some(global) => {
            let rest = configured
                .iter()
                .filter(|region| **region != global)
                .cloned()
                .collect();
            vec![vec![global], rest]
        }
        None => vec![configured.to_vec()],
    }
}

fn mark_resource_status(plan: &mut crate::TerraformPlan, indices: &[usize], status: AuditStatus) {
    for index in indices {
        if let Some(resource) = plan.resources.get_mut(*index) {
            resource.audit_attribution = Some(crate::ResourceAuditAttribution {
                status: status.clone(),
                observations: Vec::new(),
            });
        }
    }
}

fn resource_status(observations: &[AuditObservation], statuses: &[AuditStatus]) -> AuditStatus {
    if statuses.iter().any(|status| {
        matches!(
            status,
            AuditStatus::Failed
                | AuditStatus::Unavailable
                | AuditStatus::Incomplete
                | AuditStatus::PartialRetention
                | AuditStatus::OutsideRetention
                | AuditStatus::CrossAccountUnavailable
        )
    }) {
        return statuses
            .iter()
            .find(|status| {
                matches!(
                    status,
                    AuditStatus::Failed
                        | AuditStatus::Unavailable
                        | AuditStatus::Incomplete
                        | AuditStatus::PartialRetention
                        | AuditStatus::OutsideRetention
                        | AuditStatus::CrossAccountUnavailable
                )
            })
            .cloned()
            .unwrap_or(AuditStatus::Incomplete);
    }
    if !observations.is_empty() {
        AuditStatus::Complete
    } else {
        AuditStatus::NoMatch
    }
}

fn report_status(
    observations: &[AuditObservation],
    lookup_statuses: &[AuditStatus],
    has_unsupported: bool,
    has_missing_identity: bool,
) -> AuditStatus {
    if let Some(status) = lookup_statuses.iter().find(|status| {
        matches!(
            status,
            AuditStatus::Failed
                | AuditStatus::Unavailable
                | AuditStatus::Incomplete
                | AuditStatus::PartialRetention
                | AuditStatus::OutsideRetention
                | AuditStatus::CrossAccountUnavailable
        )
    }) {
        return status.clone();
    }
    if has_unsupported || has_missing_identity {
        return AuditStatus::Incomplete;
    }
    if observations.is_empty() {
        AuditStatus::NoMatch
    } else {
        AuditStatus::Complete
    }
}

fn deduplicate_observations(observations: &mut Vec<AuditObservation>) {
    observations.sort_by(|left, right| {
        let left_time = DateTime::parse_from_rfc3339(&left.event_time).ok();
        let right_time = DateTime::parse_from_rfc3339(&right.event_time).ok();
        left_time
            .cmp(&right_time)
            .then_with(|| left.event_id.cmp(&right.event_id))
            .then_with(|| left.principal_arn.cmp(&right.principal_arn))
            .then_with(|| left.account_id.cmp(&right.account_id))
            .then_with(|| left.region.cmp(&right.region))
    });
    observations.dedup_by(|left, right| {
        left.event_id == right.event_id
            && left.account_id == right.account_id
            && left.region == right.region
    });
}

/// Parse a positive compact duration accepted by author lookup options.
pub fn parse_author_window(value: &str) -> crate::Result<Duration> {
    let (unit_start, unit_character) = value
        .char_indices()
        .last()
        .ok_or_else(|| argument_error("--author-window must not be empty"))?;
    let digits = &value[..unit_start];
    let multiplier = match unit_character {
        's' => 1_u64,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 60 * 60 * 24,
        _ => {
            return Err(argument_error(
                "--author-window must be a positive duration such as 5m or 1h, at most 24h",
            ));
        }
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(argument_error(
            "--author-window must be a positive duration such as 5m or 1h, at most 24h",
        ));
    }
    let amount = digits.parse::<u64>().map_err(|_| {
        argument_error("--author-window must be a positive duration no greater than 24h")
    })?;
    let seconds = amount
        .checked_mul(multiplier)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| {
            argument_error("--author-window must be a positive duration no greater than 24h")
        })?;
    let duration = Duration::from_secs(seconds);
    if duration > MAX_AUTHOR_WINDOW {
        return Err(argument_error("--author-window must not exceed 24h"));
    }
    Ok(duration)
}

/// Build the inclusive CloudTrail interval ending at the right snapshot's S3 LastModified.
pub fn audit_window(
    last_modified: &str,
    window: Duration,
) -> crate::Result<(DateTime<Utc>, DateTime<Utc>)> {
    let end = DateTime::parse_from_rfc3339(last_modified)
        .map_err(|_| argument_error("right-side S3 LastModified is not a valid RFC3339 timestamp"))?
        .with_timezone(&Utc);
    let seconds = i64::try_from(window.as_secs())
        .map_err(|_| argument_error("--author-window is outside the supported range"))?;
    let start = end
        .checked_sub_signed(chrono::Duration::seconds(seconds))
        .ok_or_else(|| {
            argument_error("author lookup window is outside the supported date range")
        })?;
    Ok((start, end))
}

pub fn normalize_client_hint(user_agent: Option<&str>) -> Option<String> {
    let user_agent = user_agent?.trim();
    if user_agent.is_empty() || user_agent.len() > 512 || user_agent.chars().any(char::is_control) {
        return None;
    }
    let lowercase = user_agent.to_ascii_lowercase();
    let product = if lowercase.contains("opentofu") {
        "OpenTofu"
    } else if lowercase.contains("terraform") {
        "Terraform"
    } else {
        return None;
    };
    let version = user_agent.split_whitespace().find_map(|part| {
        let (name, version) = part.split_once('/')?;
        (name.eq_ignore_ascii_case(product) && safe_version(version)).then_some(version)
    });
    Some(match version {
        Some(version) => format!("{product}/{version}"),
        None => product.to_string(),
    })
}

pub fn normalize_cloudtrail_event(
    event: &aws_sdk_cloudtrail::types::Event,
) -> Option<NormalizedEvent> {
    let raw = event.cloud_trail_event()?;
    if raw.len() > 256 * 1024 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let detail = value.as_object()?;
    let recipient_account_id = detail
        .get("recipientAccountId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_account_id(value))?;
    let event_region = detail
        .get("awsRegion")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| safe_bounded_text(value, 64))?;
    let event_category = detail
        .get("eventCategory")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| safe_bounded_text(value, 64))?;
    let event_id = safe_bounded_text(event.event_id()?, 256)?.to_string();
    let event_time = DateTime::parse_from_rfc3339(
        &event
            .event_time()?
            .fmt(aws_smithy_types::date_time::Format::DateTimeWithOffset)
            .ok()?,
    )
    .ok()?
    .with_timezone(&Utc);
    let event_name = safe_bounded_text(event.event_name()?, 256)?.to_string();
    let event_source = safe_bounded_text(event.event_source()?, 256)?.to_ascii_lowercase();
    let read_only = detail
        .get("readOnly")
        .and_then(serde_json::Value::as_bool)
        .or_else(
            || match detail.get("readOnly").and_then(serde_json::Value::as_str) {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            },
        )?;
    let user_identity = detail
        .get("userIdentity")
        .and_then(serde_json::Value::as_object);
    let principal_type = user_identity
        .and_then(|identity| identity.get("type"))
        .and_then(serde_json::Value::as_str)
        .and_then(safe_identity_field)
        .map(str::to_string);
    let principal_arn = user_identity
        .and_then(|identity| identity.get("arn"))
        .and_then(serde_json::Value::as_str)
        .and_then(safe_arn)
        .map(str::to_string);
    let principal_id = user_identity
        .and_then(|identity| identity.get("principalId"))
        .and_then(serde_json::Value::as_str)
        .and_then(safe_identity_field)
        .map(str::to_string);
    let username = event
        .username()
        .and_then(safe_identity_field)
        .map(str::to_string);
    let source_identity = user_identity
        .and_then(|identity| identity.get("sessionContext"))
        .and_then(serde_json::Value::as_object)
        .and_then(|session| session.get("sourceIdentity"))
        .and_then(serde_json::Value::as_str)
        .and_then(safe_identity_field)
        .map(str::to_string);
    let session_label = principal_arn
        .as_deref()
        .and_then(|arn| arn.rsplit('/').next())
        .filter(|label| !label.is_empty())
        .map(str::to_string);
    let (email_hint, identity_hint_origin) = [
        (
            source_identity.as_deref(),
            "userIdentity.sessionContext.sourceIdentity",
        ),
        (session_label.as_deref(), "userIdentity.arn session label"),
        (username.as_deref(), "CloudTrail username"),
    ]
    .into_iter()
    .find_map(|(candidate, origin)| {
        candidate
            .filter(|candidate| looks_like_email(candidate))
            .map(|candidate| (Some(candidate.to_string()), Some(origin.to_string())))
    })
    .unwrap_or((None, None));
    let user_agent = detail
        .get("userAgent")
        .and_then(serde_json::Value::as_str)
        .and_then(|agent| safe_bounded_text(agent, 512))
        .map(str::to_string);
    let client_hint = normalize_client_hint(user_agent.as_deref());
    let resources = event
        .resources()
        .iter()
        .map(|resource| NormalizedResource {
            resource_type: resource
                .resource_type()
                .and_then(|value| safe_bounded_text(value, 256))
                .map(str::to_string),
            resource_name: resource
                .resource_name()
                .and_then(|value| safe_bounded_text(value, 512))
                .map(str::to_string),
        })
        .collect();
    let event_region = event_region.to_string();
    let event_account = recipient_account_id.to_string();
    let event_category = event_category.to_string();
    let request_parameters = detail.get("requestParameters").cloned();
    let response_elements = detail.get("responseElements").cloned();
    let observation = AuditObservation {
        event_id: event_id.clone(),
        event_time: event_time.to_rfc3339(),
        event_name: event_name.clone(),
        event_source: event_source.clone(),
        account_id: Some(event_account.clone()),
        region: Some(event_region.clone()),
        principal_type,
        principal_arn,
        principal_id,
        username,
        source_identity,
        session_label,
        email_hint,
        identity_hint_origin,
        user_agent,
        client_hint,
        match_basis: String::new(),
    };
    Some(NormalizedEvent {
        event_id,
        event_time,
        event_name,
        event_source,
        event_category: Some(event_category),
        account_id: Some(event_account),
        region: Some(event_region),
        read_only: Some(read_only),
        error_code: detail
            .get("errorCode")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| safe_bounded_text(value, 256))
            .map(str::to_string),
        resources,
        request_parameters,
        response_elements,
        observation,
    })
}

fn safe_identity_field(value: &str) -> Option<&str> {
    safe_bounded_text(value, 512).filter(|value| !value.is_empty())
}

fn safe_arn(value: &str) -> Option<&str> {
    let value = safe_identity_field(value)?;
    let mut pieces = value.splitn(6, ':');
    (pieces.next() == Some("arn")
        && pieces
            .next()
            .is_some_and(|partition| partition.starts_with("aws"))
        && pieces.next().is_some_and(|service| !service.is_empty())
        && pieces
            .next()
            .is_some_and(|region| region.is_empty() || safe_region(region))
        && pieces
            .next()
            .is_some_and(|account| account.is_empty() || valid_account_id(account))
        && pieces.next().is_some_and(|resource| !resource.is_empty()))
    .then_some(value)
}

fn safe_bounded_text(value: &str, maximum: usize) -> Option<&str> {
    (!value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control))
        .then_some(value)
}

fn safe_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

fn looks_like_email(value: &str) -> bool {
    if value.len() > 254 || value.chars().any(char::is_control) {
        return false;
    }
    let Some((local, domain)) = value.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !local.contains(' ')
        && !domain.contains(' ')
}

fn valid_account_id(value: &str) -> bool {
    value.len() == 12 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn safe_region(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn argument_error(message: &str) -> crate::TfdiffError {
    crate::TfdiffError::ParseError(format!("Invalid author options: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::provider::SharedCredentialsProvider;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::thread;

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.is_empty() || line == "\r\n" {
                request.push_str("\r\n");
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            request.push_str(&line);
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        request.push_str(std::str::from_utf8(&body).unwrap());
        request
    }

    struct MockTransport {
        pages: Mutex<VecDeque<std::result::Result<LookupPage, AuditTransportError>>>,
        requests: Mutex<Vec<LookupRequest>>,
        identity_requests: Mutex<usize>,
    }

    #[async_trait]
    impl AuditTransport for MockTransport {
        async fn caller_identity(
            &self,
        ) -> std::result::Result<CallerIdentity, AuditTransportError> {
            *self.identity_requests.lock().unwrap() += 1;
            Ok(CallerIdentity {
                account_id: "111122223333".to_string(),
                arn: "arn:aws:iam::111122223333:user/test".to_string(),
                user_id: "test-user".to_string(),
            })
        }

        async fn lookup_events(
            &self,
            request: LookupRequest,
        ) -> std::result::Result<LookupPage, AuditTransportError> {
            self.requests.lock().unwrap().push(request);
            self.pages.lock().unwrap().pop_front().unwrap_or_else(|| {
                Ok(LookupPage {
                    events: Vec::new(),
                    next_token: None,
                    malformed_events: 0,
                })
            })
        }
    }

    fn mock_transport(
        pages: impl IntoIterator<Item = std::result::Result<LookupPage, AuditTransportError>>,
    ) -> MockTransport {
        MockTransport {
            pages: Mutex::new(pages.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            identity_requests: Mutex::new(0),
        }
    }

    fn local_aws_config(endpoint: String) -> aws_config::SdkConfig {
        let credentials = aws_credential_types::Credentials::new(
            "offline-test-access",
            "offline-test-secret",
            None,
            None,
            "local-audit-test",
        );
        aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .credentials_provider(SharedCredentialsProvider::new(credentials))
            .retry_config(aws_smithy_types::retry::RetryConfig::standard().with_max_attempts(1))
            .build()
    }

    fn local_service_mock(
        expected_requests: usize,
        route: impl Fn(String) -> (u16, String, String) + Send + Sync + 'static,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let route = std::sync::Arc::new(route);
        let worker = thread::spawn(move || {
            let mut requests = Vec::with_capacity(expected_requests);
            for _ in 0..expected_requests {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            panic!("timed out waiting for local AWS request");
                        }
                        Err(error) => panic!("local AWS mock accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = read_http_request(&mut stream);
                let (status, content_type, body) = route(request.clone());
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    403 => "Forbidden",
                    _ => "Internal Server Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        });
        (endpoint, worker)
    }

    fn event() -> NormalizedEvent {
        let observation = AuditObservation {
            event_id: "event-1".to_string(),
            event_time: "2026-10-12T12:00:00Z".to_string(),
            event_name: "CreateVpcEndpoint".to_string(),
            event_source: "ec2.amazonaws.com".to_string(),
            account_id: Some("111122223333".to_string()),
            region: Some("eu-central-1".to_string()),
            principal_type: None,
            principal_arn: None,
            principal_id: None,
            username: None,
            source_identity: None,
            session_label: None,
            email_hint: None,
            identity_hint_origin: None,
            user_agent: None,
            client_hint: None,
            match_basis: String::new(),
        };
        NormalizedEvent {
            event_id: "event-1".to_string(),
            event_time: DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            event_name: "CreateVpcEndpoint".to_string(),
            event_source: "ec2.amazonaws.com".to_string(),
            event_category: Some("Management".to_string()),
            account_id: Some("111122223333".to_string()),
            region: Some("eu-central-1".to_string()),
            read_only: Some(false),
            error_code: None,
            resources: Vec::new(),
            request_parameters: None,
            response_elements: None,
            observation,
        }
    }

    fn endpoint_resource(
        action: crate::ActionType,
        before_id: Option<&str>,
        after_id: Option<&str>,
        sensitive_id: bool,
    ) -> crate::Resource {
        let mut attributes = std::collections::HashMap::new();
        if let Some(id) = after_id.or(before_id) {
            attributes.insert("id".to_string(), serde_json::json!(id));
        }
        crate::Resource {
            id: "aws_vpc_endpoint.test".to_string(),
            plan_delta: None,
            name: "test".to_string(),
            type_name: "aws_vpc_endpoint".to_string(),
            provider: "registry.terraform.io/hashicorp/aws".to_string(),
            action,
            changes: vec![crate::Change {
                path: vec!["id".to_string()],
                before: before_id.map(|id| serde_json::json!(id)),
                after: after_id.map(|id| serde_json::json!(id)),
                sensitive: sensitive_id,
                computed: false,
            }],
            attributes,
            audit_attribution: None,
            applied: false,
            source: None,
        }
    }

    #[test]
    fn matches_vpc_endpoint_create_modify_and_destroy_by_exact_structured_id() {
        let created_id = "vpce-0123456789abcdef0";
        let create_resource =
            endpoint_resource(crate::ActionType::Create, None, Some(created_id), false);
        let mut create_event = event();
        create_event.response_elements = Some(serde_json::json!({
            "vpcEndpoint": { "vpcEndpointId": created_id }
        }));
        let observation = match_resource_event(
            &create_resource,
            &create_event,
            "111122223333",
            "eu-central-1",
        )
        .unwrap();
        assert_eq!(
            observation.match_basis,
            "responseElements.vpcEndpoint.vpcEndpointId"
        );

        let existing_id = "vpce-abcdef01234567890";
        let update_resource = endpoint_resource(
            crate::ActionType::Update,
            Some(existing_id),
            Some(existing_id),
            false,
        );
        let mut modify_event = event();
        modify_event.event_name = "ModifyVpcEndpoint".to_string();
        modify_event.request_parameters = Some(serde_json::json!({ "vpcEndpointId": existing_id }));
        assert_eq!(
            match_resource_event(
                &update_resource,
                &modify_event,
                "111122223333",
                "eu-central-1"
            )
            .unwrap()
            .match_basis,
            "requestParameters.vpcEndpointId"
        );

        let destroyed_id = "vpce-1234567890abcdef0";
        let destroy_resource =
            endpoint_resource(crate::ActionType::Destroy, Some(destroyed_id), None, false);
        let mut delete_event = event();
        delete_event.event_name = "DeleteVpcEndpoints".to_string();
        delete_event.request_parameters = Some(serde_json::json!({
            "vpcEndpointIds": [destroyed_id]
        }));
        assert_eq!(
            match_resource_event(
                &destroy_resource,
                &delete_event,
                "111122223333",
                "eu-central-1"
            )
            .unwrap()
            .match_basis,
            "requestParameters.vpcEndpointIds"
        );
        delete_event.request_parameters = Some(serde_json::json!({
            "vpcEndpointIds": ["vpce-fffffffffffffffff"]
        }));
        assert!(match_resource_event(
            &destroy_resource,
            &delete_event,
            "111122223333",
            "eu-central-1"
        )
        .is_none());
    }

    #[test]
    fn delete_endpoint_matching_excludes_unsuccessful_ids_from_partial_responses() {
        let successful_id = "vpce-0123456789abcdef0";
        let failed_id = "vpce-abcdef01234567890";
        let mut event = event();
        event.event_name = "DeleteVpcEndpoints".to_string();
        event.request_parameters = Some(serde_json::json!({
            "vpcEndpointIds": [successful_id, failed_id]
        }));
        event.response_elements = Some(serde_json::json!({
            "unsuccessful": [{ "resourceId": failed_id, "errorCode": "OperationNotPermitted" }]
        }));

        let successful =
            endpoint_resource(crate::ActionType::Destroy, Some(successful_id), None, false);
        let failed = endpoint_resource(crate::ActionType::Destroy, Some(failed_id), None, false);
        assert!(
            match_resource_event(&successful, &event, "111122223333", "eu-central-1").is_some()
        );
        assert!(match_resource_event(&failed, &event, "111122223333", "eu-central-1").is_none());
    }

    #[test]
    fn modify_endpoint_matching_rejects_explicit_unsuccessful_response() {
        let id = "vpce-0123456789abcdef0";
        let resource = endpoint_resource(crate::ActionType::Update, Some(id), Some(id), false);
        let mut event = event();
        event.event_name = "ModifyVpcEndpoint".to_string();
        event.request_parameters = Some(serde_json::json!({ "vpcEndpointId": id }));
        event.response_elements = Some(serde_json::json!({ "return": false }));

        assert!(match_resource_event(&resource, &event, "111122223333", "eu-central-1").is_none());
    }

    #[test]
    fn vpc_endpoint_matching_rejects_wrong_scope_unsupported_or_unverified_events() {
        let id = "vpce-0123456789abcdef0";
        let resource = endpoint_resource(crate::ActionType::Create, None, Some(id), false);
        let mut event = event();
        event.response_elements = Some(serde_json::json!({
            "vpcEndpoint": { "vpcEndpointId": id }
        }));
        assert!(match_resource_event(&resource, &event, "999988887777", "eu-central-1").is_none());
        assert!(match_resource_event(&resource, &event, "111122223333", "us-east-1").is_none());

        let mut unsupported = resource.clone();
        unsupported.type_name = "aws_instance".to_string();
        assert!(
            match_resource_event(&unsupported, &event, "111122223333", "eu-central-1").is_none()
        );
        event.event_name = "RunInstances".to_string();
        assert!(match_resource_event(&resource, &event, "111122223333", "eu-central-1").is_none());

        event.event_name = "CreateVpcEndpoint".to_string();
        event.read_only = Some(true);
        assert!(match_resource_event(&resource, &event, "111122223333", "eu-central-1").is_none());
        event.read_only = None;
        assert!(match_resource_event(&resource, &event, "111122223333", "eu-central-1").is_none());
        event.read_only = Some(false);
        event.event_category = Some("Data".to_string());
        assert!(match_resource_event(&resource, &event, "111122223333", "eu-central-1").is_none());
        event.event_category = Some("Management".to_string());
        event.error_code = Some("AccessDenied".to_string());
        assert!(match_resource_event(&resource, &event, "111122223333", "eu-central-1").is_none());
    }

    #[test]
    fn vpc_endpoint_matching_rejects_sensitive_id_and_unknown_create_identity() {
        let id = "vpce-0123456789abcdef0";
        let sensitive = endpoint_resource(crate::ActionType::Create, None, Some(id), true);
        let mut event = event();
        event.response_elements = Some(serde_json::json!({
            "vpcEndpoint": { "vpcEndpointId": id }
        }));
        assert!(match_resource_event(&sensitive, &event, "111122223333", "eu-central-1").is_none());

        let unknown = endpoint_resource(crate::ActionType::Create, None, None, false);
        assert!(match_resource_event(&unknown, &event, "111122223333", "eu-central-1").is_none());
    }

    #[tokio::test]
    async fn aws_transport_uses_local_endpoints_and_exact_lookup_request_shape() {
        let (endpoint, server) = local_service_mock(2, |request| {
            if request.starts_with("POST /") && request.contains("Action=GetCallerIdentity") {
                (
                    200,
                    "text/xml".to_string(),
                    "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Arn>arn:aws:iam::111122223333:user/test</Arn><UserId>test-user</UserId><Account>111122223333</Account></GetCallerIdentityResult><ResponseMetadata><RequestId>local</RequestId></ResponseMetadata></GetCallerIdentityResponse>".to_string(),
                )
            } else if request.starts_with("POST /")
                && request
                    .to_ascii_lowercase()
                    .contains("x-amz-target: cloudtrail_20131101.lookupevents")
            {
                (
                    200,
                    "application/x-amz-json-1.1".to_string(),
                    serde_json::json!({"Events":[{"EventId":"event-1","EventName":"CreateVpcEndpoint","EventSource":"ec2.amazonaws.com","EventTime":1791806400,"CloudTrailEvent":r#"{"eventTime":"2026-10-12T12:00:00Z","awsRegion":"eu-central-1","recipientAccountId":"111122223333","eventCategory":"Management","readOnly":false}"#}],"NextToken":"next-token"}).to_string(),
                )
            } else {
                panic!("unexpected local AWS request: {request}");
            }
        });
        let config = local_aws_config(endpoint.clone());
        assert_eq!(config.endpoint_url(), Some(endpoint.as_str()));
        let transport = AwsAuditTransport::from_sdk_config(&config);
        let caller = transport.caller_identity().await.unwrap();
        assert_eq!(caller.account_id, "111122223333");
        assert_eq!(caller.arn, "arn:aws:iam::111122223333:user/test");

        let start = DateTime::parse_from_rfc3339("2026-10-12T11:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let page = transport
            .lookup_events(LookupRequest {
                region: "eu-central-1".to_string(),
                start_time: start,
                end_time: end,
                next_token: None,
                page_size: 50,
            })
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.next_token.as_deref(), Some("next-token"));
        assert_eq!(page.malformed_events, 0);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        let sts = &requests[0];
        assert!(
            sts.to_ascii_lowercase()
                .contains("action=getcalleridentity"),
            "{sts}"
        );
        let cloudtrail = &requests[1];
        assert!(
            cloudtrail
                .to_ascii_lowercase()
                .contains("x-amz-target: cloudtrail_20131101.lookupevents"),
            "{cloudtrail}"
        );
        let body = cloudtrail.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body["LookupAttributes"],
            serde_json::json!([{"AttributeKey":"ReadOnly","AttributeValue":"false"}])
        );
        assert_eq!(body["MaxResults"], 50);
        assert_eq!(body["StartTime"], start.timestamp());
        assert_eq!(body["EndTime"], end.timestamp());
        assert!(body.get("NextToken").is_none());
    }

    #[tokio::test]
    async fn aws_transport_forwards_continuation_token() {
        let (endpoint, server) = local_service_mock(1, |_request| {
            (
                200,
                "application/x-amz-json-1.1".to_string(),
                r#"{"Events":[],"NextToken":"next-token"}"#.to_string(),
            )
        });
        let transport = AwsAuditTransport::from_sdk_config(&local_aws_config(endpoint));
        let start = DateTime::parse_from_rfc3339("2026-10-12T11:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let page = transport
            .lookup_events(LookupRequest {
                region: "eu-central-1".to_string(),
                start_time: start,
                end_time: end,
                next_token: Some("next-token-1".to_string()),
                page_size: 50,
            })
            .await
            .unwrap();
        assert_eq!(page.next_token.as_deref(), Some("next-token"));
        let requests = server.join().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["NextToken"], "next-token-1");
    }

    #[tokio::test]
    async fn transport_normalizes_empty_continuation_token_to_end_of_pages() {
        let (endpoint, server) = local_service_mock(1, |_request| {
            (
                200,
                "application/x-amz-json-1.1".to_string(),
                r#"{"Events":[],"NextToken":""}"#.to_string(),
            )
        });
        let transport = AwsAuditTransport::from_sdk_config(&local_aws_config(endpoint));
        let start = DateTime::parse_from_rfc3339("2026-10-12T11:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let page = transport
            .lookup_events(LookupRequest {
                region: "eu-central-1".to_string(),
                start_time: start,
                end_time: end,
                next_token: None,
                page_size: 50,
            })
            .await
            .unwrap();
        assert_eq!(page.next_token, None);
        assert!(server.join().unwrap().len() == 1);
    }

    #[tokio::test]
    async fn skips_queries_for_windows_outside_retention() {
        let transport = mock_transport([]);
        let caller = transport.caller_identity().await.unwrap();
        let outcome = query_event_history(
            &transport,
            &caller,
            "eu-central-1",
            DateTime::parse_from_rfc3339("2026-05-08T11:59:53Z")
                .unwrap()
                .with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2026-05-08T12:04:53Z")
                .unwrap()
                .with_timezone(&Utc),
            50,
            DateTime::parse_from_rfc3339("2026-10-13T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .await;
        assert_eq!(outcome.status, AuditStatus::OutsideRetention);
        assert_eq!(outcome.pages, 0);
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_enrichment_window_skips_identity_and_audit_requests() {
        let last_modified = "2026-05-08T12:04:53Z";
        let mut plan = crate::TerraformPlan {
            mode: crate::PlanMode::StateDiff,
            source_versions: Some(crate::SourceVersions {
                left_label: "left".to_string(),
                right_label: "right".to_string(),
                left: None,
                right: Some(crate::SourceVersion {
                    version_id: Some("version-right".to_string()),
                    last_modified: Some(last_modified.to_string()),
                    etag: None,
                }),
            }),
            resources: vec![endpoint_resource(
                crate::ActionType::Create,
                None,
                Some("vpce-0123456789abcdef0"),
                false,
            )],
            ..Default::default()
        };
        let transport = mock_transport([]);
        let now = DateTime::parse_from_rfc3339("2026-10-13T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let options = AuthorOptions {
            show_authors: true,
            show_atomic_authors: false,
            regions: vec!["eu-central-1".to_string()],
            log_groups: Vec::new(),
            window: DEFAULT_AUTHOR_WINDOW,
            page_size: DEFAULT_CLOUDTRAIL_PAGE_SIZE,
            iam_region: None,
            global_region: None,
            auxiliary_options_provided: true,
        };

        let report = enrich_plan_with_transport(
            &mut plan,
            &options,
            Some(&transport),
            Some("eu-central-1"),
            now,
        )
        .await;

        assert_eq!(report.status, AuditStatus::OutsideRetention);
        assert_eq!(*transport.identity_requests.lock().unwrap(), 0);
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn queries_inclusive_retained_window_and_filters_invalid_events() {
        let mut wrong_account = event();
        wrong_account.account_id = Some("999988887777".to_string());
        let mut failed = event();
        failed.error_code = Some("AccessDenied".to_string());
        let mut wrong_category = event();
        wrong_category.event_category = Some("Data".to_string());
        let transport = mock_transport([
            Ok(LookupPage {
                events: vec![event(), wrong_account, failed, wrong_category],
                next_token: Some("next".to_string()),
                malformed_events: 0,
            }),
            Ok(LookupPage {
                events: vec![event()],
                next_token: None,
                malformed_events: 0,
            }),
        ]);
        let caller = transport.caller_identity().await.unwrap();
        let start = DateTime::parse_from_rfc3339("2026-10-12T11:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339("2026-10-13T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let outcome =
            query_event_history(&transport, &caller, "eu-central-1", start, end, 50, now).await;
        assert_eq!(outcome.status, AuditStatus::Complete);
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.pages, 2);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].start_time, start);
        assert_eq!(requests[0].end_time, end);
        assert_eq!(requests[1].next_token.as_deref(), Some("next"));
    }

    #[tokio::test]
    async fn marks_repeated_tokens_and_page_caps_incomplete() {
        let repeated = mock_transport([
            Ok(LookupPage {
                events: Vec::new(),
                next_token: Some("same".to_string()),
                malformed_events: 0,
            }),
            Ok(LookupPage {
                events: Vec::new(),
                next_token: Some("same".to_string()),
                malformed_events: 0,
            }),
        ]);
        let caller = repeated.caller_identity().await.unwrap();
        let start = DateTime::parse_from_rfc3339("2026-10-12T11:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339("2026-10-13T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let outcome =
            query_event_history(&repeated, &caller, "eu-central-1", start, end, 50, now).await;
        assert_eq!(outcome.status, AuditStatus::Incomplete);
        assert!(outcome.message.as_deref().unwrap().contains("repeated"));
        assert_eq!(outcome.pages, 2);

        let drained = mock_transport((0..25).map(|index| {
            Ok(LookupPage {
                events: Vec::new(),
                next_token: (index < 24).then(|| format!("token-{index}")),
                malformed_events: 0,
            })
        }));
        let caller = drained.caller_identity().await.unwrap();
        let outcome =
            query_event_history(&drained, &caller, "eu-central-1", start, end, 7, now).await;
        assert_eq!(outcome.status, AuditStatus::NoMatch);
        assert_eq!(outcome.pages, 25);
        assert!(drained
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.page_size == 7));

        let capped = mock_transport((0..MAX_LOOKUP_PAGES).map(|index| {
            Ok(LookupPage {
                events: Vec::new(),
                next_token: Some(format!("token-{index}")),
                malformed_events: 0,
            })
        }));
        let caller = capped.caller_identity().await.unwrap();
        let outcome =
            query_event_history(&capped, &caller, "eu-central-1", start, end, 50, now).await;
        assert_eq!(outcome.status, AuditStatus::Incomplete);
        assert!(outcome.message.as_deref().unwrap().contains("limit"));
        assert_eq!(outcome.pages, MAX_LOOKUP_PAGES);
    }

    fn iam_attachment_resource(action: crate::ActionType) -> crate::Resource {
        let mut attributes = std::collections::HashMap::new();
        attributes.insert("role".to_string(), serde_json::json!("AcmeEcsTaskRole"));
        attributes.insert(
            "policy_arn".to_string(),
            serde_json::json!("arn:aws:iam::111122223333:policy/acme-federation"),
        );
        crate::Resource {
            id: "aws_iam_role_policy_attachment.this[\"AcmeEcsTaskRole\"]".to_string(),
            plan_delta: None,
            name: "this".to_string(),
            type_name: "aws_iam_role_policy_attachment".to_string(),
            provider: "registry.terraform.io/hashicorp/aws".to_string(),
            action,
            changes: Vec::new(),
            attributes,
            audit_attribution: None,
            applied: false,
            source: None,
        }
    }

    fn iam_attach_event(event_name: &str, region: &str) -> NormalizedEvent {
        let mut normalized = event();
        normalized.event_name = event_name.to_string();
        normalized.event_source = "iam.amazonaws.com".to_string();
        normalized.region = Some(region.to_string());
        normalized.request_parameters = Some(serde_json::json!({
            "roleName": "AcmeEcsTaskRole",
            "policyArn": "arn:aws:iam::111122223333:policy/acme-federation",
        }));
        normalized.observation.event_name = event_name.to_string();
        normalized.observation.event_source = "iam.amazonaws.com".to_string();
        normalized.observation.region = Some(region.to_string());
        normalized
    }

    fn iam_state_plan(resources: Vec<crate::Resource>) -> crate::TerraformPlan {
        crate::TerraformPlan {
            mode: crate::PlanMode::StateDiff,
            source_versions: Some(crate::SourceVersions {
                left_label: "left".to_string(),
                right_label: "right".to_string(),
                left: None,
                right: Some(crate::SourceVersion {
                    version_id: Some("version-right".to_string()),
                    last_modified: Some("2026-10-12T12:00:00Z".to_string()),
                    etag: None,
                }),
            }),
            resources,
            ..Default::default()
        }
    }

    fn global_options(iam_region: Option<&str>, global_region: Option<&str>) -> AuthorOptions {
        AuthorOptions {
            show_authors: true,
            show_atomic_authors: false,
            regions: vec!["eu-central-1".to_string()],
            log_groups: Vec::new(),
            window: DEFAULT_AUTHOR_WINDOW,
            page_size: DEFAULT_CLOUDTRAIL_PAGE_SIZE,
            iam_region: iam_region.map(str::to_string),
            global_region: global_region.map(str::to_string),
            auxiliary_options_provided: true,
        }
    }

    fn audit_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-13T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn page(events: Vec<NormalizedEvent>) -> std::result::Result<LookupPage, AuditTransportError> {
        Ok(LookupPage {
            events,
            next_token: None,
            malformed_events: 0,
        })
    }

    #[tokio::test]
    async fn iam_attachment_hits_us_east_1_before_configured_regions() {
        let mut plan = iam_state_plan(vec![iam_attachment_resource(crate::ActionType::Create)]);
        let transport = mock_transport([page(vec![iam_attach_event(
            "AttachRolePolicy",
            "us-east-1",
        )])]);

        enrich_plan_with_transport(
            &mut plan,
            &global_options(None, None),
            Some(&transport),
            Some("eu-central-1"),
            audit_now(),
        )
        .await;

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].region, "us-east-1");
        let attribution = plan.resources[0].audit_attribution.as_ref().unwrap();
        assert_eq!(attribution.status, AuditStatus::Complete);
        assert_eq!(attribution.observations.len(), 1);
    }

    #[tokio::test]
    async fn iam_attachment_falls_back_to_configured_region_on_global_miss() {
        let mut plan = iam_state_plan(vec![iam_attachment_resource(crate::ActionType::Create)]);
        let transport = mock_transport([
            page(Vec::new()),
            page(vec![iam_attach_event("AttachRolePolicy", "eu-central-1")]),
        ]);

        enrich_plan_with_transport(
            &mut plan,
            &global_options(None, None),
            Some(&transport),
            Some("eu-central-1"),
            audit_now(),
        )
        .await;

        let requests = transport.requests.lock().unwrap();
        let regions: Vec<&str> = requests
            .iter()
            .map(|request| request.region.as_str())
            .collect();
        assert_eq!(regions, ["us-east-1", "eu-central-1"]);
        let attribution = plan.resources[0].audit_attribution.as_ref().unwrap();
        assert_eq!(attribution.observations.len(), 1);
        assert_eq!(
            attribution.observations[0].region.as_deref(),
            Some("eu-central-1")
        );
    }

    #[tokio::test]
    async fn iam_region_override_replaces_us_east_1_as_global_lookup_region() {
        let mut plan = iam_state_plan(vec![iam_attachment_resource(crate::ActionType::Create)]);
        let transport = mock_transport([page(vec![iam_attach_event(
            "AttachRolePolicy",
            "us-west-2",
        )])]);

        enrich_plan_with_transport(
            &mut plan,
            &global_options(Some("us-west-2"), None),
            Some(&transport),
            Some("eu-central-1"),
            audit_now(),
        )
        .await;

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].region, "us-west-2");
    }

    #[tokio::test]
    async fn global_lookup_region_is_cached_per_region_across_resources() {
        let mut plan = iam_state_plan(vec![
            iam_attachment_resource(crate::ActionType::Create),
            iam_attachment_resource(crate::ActionType::Create),
        ]);
        let transport = mock_transport([page(Vec::new()), page(Vec::new())]);

        enrich_plan_with_transport(
            &mut plan,
            &global_options(None, None),
            Some(&transport),
            Some("eu-central-1"),
            audit_now(),
        )
        .await;

        let requests = transport.requests.lock().unwrap();
        let regions: Vec<&str> = requests
            .iter()
            .map(|request| request.region.as_str())
            .collect();
        assert_eq!(regions, ["us-east-1", "eu-central-1"]);
    }

    #[test]
    fn global_lookup_region_routes_iam_and_other_global_services_separately() {
        let configured = vec!["eu-central-1".to_string()];
        let iam = iam_attachment_resource(crate::ActionType::Create);
        let mut cloudfront = iam_attachment_resource(crate::ActionType::Create);
        cloudfront.type_name = "aws_cloudfront_distribution".to_string();

        assert_eq!(
            global_lookup_region(&iam, &global_options(None, None)).as_deref(),
            Some("us-east-1")
        );
        assert_eq!(
            global_lookup_region(&iam, &global_options(None, Some("ap-south-1"))).as_deref(),
            Some("us-east-1")
        );
        assert_eq!(
            global_lookup_region(&iam, &global_options(Some("us-west-2"), Some("ap-south-1")))
                .as_deref(),
            Some("us-west-2")
        );
        assert_eq!(
            global_lookup_region(&cloudfront, &global_options(None, None)).as_deref(),
            Some("us-east-1")
        );
        assert_eq!(
            global_lookup_region(
                &cloudfront,
                &global_options(Some("us-west-2"), Some("ap-south-1"))
            )
            .as_deref(),
            Some("ap-south-1")
        );

        assert_eq!(
            lookup_phases(&iam, &global_options(None, None), &configured),
            vec![
                vec!["us-east-1".to_string()],
                vec!["eu-central-1".to_string()]
            ]
        );
        assert_eq!(
            lookup_phases(
                &iam,
                &global_options(Some("eu-central-1"), None),
                &configured
            ),
            vec![vec!["eu-central-1".to_string()], Vec::<String>::new()]
        );
    }

    #[test]
    fn validate_checks_global_region_overrides() {
        assert!(global_options(Some("us-east-1"), Some("eu-west-1"))
            .validate()
            .is_ok());
        assert!(global_options(Some("US East"), None).validate().is_err());
        assert!(global_options(None, Some("eu/west")).validate().is_err());

        let mut disabled = global_options(Some("us-east-1"), None);
        disabled.show_authors = false;
        assert!(disabled.validate().is_err());
    }

    #[test]
    fn identity_hints_containing_password_or_secret_are_retained_as_safe_text() {
        assert_eq!(
            safe_identity_field("password-reset-role"),
            Some("password-reset-role")
        );
        assert_eq!(
            safe_identity_field("secretary@example.test"),
            Some("secretary@example.test")
        );
    }

    #[test]
    fn accepts_only_positive_compact_windows_up_to_one_day() {
        for (value, seconds) in [
            ("1s", 1),
            ("5m", 300),
            ("1h", 3600),
            ("24h", 86400),
            ("1d", 86400),
        ] {
            assert_eq!(parse_author_window(value).unwrap().as_secs(), seconds);
        }
        for value in ["0s", "-1m", "25h", "1w", "", "5", "1.5h"] {
            assert!(parse_author_window(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn requires_author_flag_for_auxiliary_options() {
        let mut options = AuthorOptions {
            show_authors: false,
            show_atomic_authors: false,
            regions: Vec::new(),
            log_groups: Vec::new(),
            window: DEFAULT_AUTHOR_WINDOW,
            page_size: DEFAULT_CLOUDTRAIL_PAGE_SIZE,
            iam_region: None,
            global_region: None,
            auxiliary_options_provided: false,
        };
        assert!(options.validate().is_ok());
        options.auxiliary_options_provided = true;
        options.regions.push("eu-central-1".to_string());
        assert!(options.validate().is_err());
        options.show_authors = true;
        assert!(options.validate().is_ok());
    }

    #[test]
    fn rejects_page_sizes_outside_cloudtrail_range() {
        let mut options = AuthorOptions {
            show_authors: true,
            show_atomic_authors: false,
            regions: Vec::new(),
            log_groups: Vec::new(),
            window: DEFAULT_AUTHOR_WINDOW,
            page_size: 0,
            iam_region: None,
            global_region: None,
            auxiliary_options_provided: true,
        };
        assert!(options.validate().is_err());
        options.page_size = MAX_CLOUDTRAIL_PAGE_SIZE + 1;
        assert!(options.validate().is_err());
        options.page_size = 1;
        assert!(options.validate().is_ok());
        options.page_size = MAX_CLOUDTRAIL_PAGE_SIZE;
        assert!(options.validate().is_ok());
    }

    #[test]
    fn computes_window_from_source_timestamp_without_local_clock() {
        let (start, end) = audit_window("2026-05-08T12:04:53Z", Duration::from_secs(300)).unwrap();
        assert_eq!(start.to_rfc3339(), "2026-05-08T11:59:53+00:00");
        assert_eq!(end.to_rfc3339(), "2026-05-08T12:04:53+00:00");
    }

    #[test]
    fn missing_observation_fields_are_omitted_from_json() {
        let observation = AuditObservation {
            event_id: "event-id".to_string(),
            event_time: "2026-10-13T00:00:00Z".to_string(),
            event_name: "CreateVpcEndpoint".to_string(),
            event_source: "ec2.amazonaws.com".to_string(),
            account_id: None,
            region: None,
            principal_type: None,
            principal_arn: None,
            principal_id: None,
            username: None,
            source_identity: None,
            session_label: None,
            email_hint: None,
            identity_hint_origin: None,
            user_agent: None,
            client_hint: None,
            match_basis: "response id".to_string(),
        };
        let value = serde_json::to_value(observation).unwrap();
        assert!(value.get("principal_arn").is_none());
        assert!(value.get("email_hint").is_none());
        assert!(value.get("user_agent").is_none());
    }

    #[test]
    fn extracts_only_bounded_terraform_client_hints() {
        assert_eq!(
            normalize_client_hint(Some("Terraform/1.11.5 (+https://terraform.io)")),
            Some("Terraform/1.11.5".to_string())
        );
        assert_eq!(
            normalize_client_hint(Some("OpenTofu/1.9.0")),
            Some("OpenTofu/1.9.0".to_string())
        );
        assert_eq!(normalize_client_hint(Some("aws-cli/2.0")), None);
        assert_eq!(normalize_client_hint(Some("Terraform/1.2\nforged")), None);
    }

    #[test]
    fn cloudtrail_normalization_rejects_missing_required_event_metadata() {
        let required = serde_json::json!({
            "eventTime": "2026-10-12T12:00:00Z",
            "awsRegion": "eu-central-1",
            "recipientAccountId": "111122223333",
            "eventCategory": "Management",
            "readOnly": false
        });
        for field in [
            "awsRegion",
            "recipientAccountId",
            "eventCategory",
            "readOnly",
        ] {
            let mut detail = required.clone();
            detail.as_object_mut().unwrap().remove(field);
            let event = aws_sdk_cloudtrail::types::Event::builder()
                .event_id("event-123")
                .event_time(aws_smithy_types::DateTime::from_secs(1_792_000_800))
                .event_name("CreateVpcEndpoint")
                .event_source("ec2.amazonaws.com")
                .cloud_trail_event(serde_json::to_string(&detail).unwrap())
                .build();
            assert!(
                normalize_cloudtrail_event(&event).is_none(),
                "accepted missing {field}"
            );
        }
    }

    #[test]
    fn cloudtrail_normalization_keeps_only_bounded_parsed_evidence() {
        let event = aws_sdk_cloudtrail::types::Event::builder()
            .event_id("event-123")
            .event_time(aws_smithy_types::DateTime::from_secs(1_792_000_800))
            .event_name("CreateVpcEndpoint")
            .event_source("ec2.amazonaws.com")
            .read_only("false")
            .cloud_trail_event(
                r#"{
                    "eventVersion":"1.09",
                    "eventTime":"2026-10-12T12:00:00Z",
                    "eventSource":"ec2.amazonaws.com",
                    "eventName":"CreateVpcEndpoint",
                    "awsRegion":"eu-central-1",
                    "recipientAccountId":"111122223333",
                    "eventCategory":"Management",
                    "readOnly":false,
                    "userIdentity":{"type":"AssumedRole","arn":"arn:aws:sts::111122223333:assumed-role/Deploy/alex@example.test","principalId":"AROA123:alex@example.test","sessionContext":{"sourceIdentity":"alex@example.test"}},
                    "userAgent":"Terraform/1.11.5",
                    "requestParameters":{"vpcEndpointType":"Interface"},
                    "responseElements":{"vpcEndpoint":{"vpcEndpointId":"vpce-123"}}
                }"#,
            )
            .build();
        let normalized = normalize_cloudtrail_event(&event).unwrap();
        assert_eq!(normalized.event_category.as_deref(), Some("Management"));
        assert_eq!(normalized.account_id.as_deref(), Some("111122223333"));
        assert_eq!(normalized.region.as_deref(), Some("eu-central-1"));
        assert_eq!(
            normalized.observation.client_hint.as_deref(),
            Some("Terraform/1.11.5")
        );
        assert_eq!(
            normalized.observation.email_hint.as_deref(),
            Some("alex@example.test")
        );
        assert_eq!(
            normalized.response_elements.as_ref().unwrap()["vpcEndpoint"]["vpcEndpointId"],
            "vpce-123"
        );

        let oversized = aws_sdk_cloudtrail::types::Event::builder()
            .event_id("event-123")
            .event_time(aws_smithy_types::DateTime::from_secs(1_792_000_800))
            .event_name("CreateVpcEndpoint")
            .event_source("ec2.amazonaws.com")
            .cloud_trail_event("x".repeat(256 * 1024 + 1))
            .build();
        assert!(normalize_cloudtrail_event(&oversized).is_none());
    }
}
