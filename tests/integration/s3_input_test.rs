use assert_cmd::Command;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

struct MockS3 {
    endpoint: String,
    requests: JoinHandle<Vec<String>>,
}

impl MockS3 {
    fn start(responses: impl IntoIterator<Item = String>) -> Self {
        let responses: Vec<String> = responses.into_iter().collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = thread::spawn(move || {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            return requests;
                        }
                        Err(error) => panic!("mock S3 accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut stream);
                requests.push(request);
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        Self { endpoint, requests }
    }

    fn start_routed(
        expected_requests: usize,
        route: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let route = std::sync::Arc::new(route);
        let requests = thread::spawn(move || {
            let mut workers = Vec::with_capacity(expected_requests);
            for _ in 0..expected_requests {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            panic!("timed out waiting for mock S3 requests");
                        }
                        Err(error) => panic!("mock S3 accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let route = route.clone();
                workers.push(thread::spawn(move || {
                    let request = read_request(&mut stream);
                    let response = route(&request);
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.flush().unwrap();
                    request
                }));
            }
            workers
                .into_iter()
                .map(|worker| worker.join().expect("mock S3 request worker panicked"))
                .collect()
        });
        Self { endpoint, requests }
    }

    fn finish(self) -> Vec<String> {
        self.requests
            .join()
            .expect("mock S3 server thread panicked")
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.is_empty() || line == "\r\n" {
            break;
        }
        request.push_str(&line);
    }
    request
}

fn response(status: &str, body: &str, extra_headers: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{body}",
        body.len()
    )
}

fn object_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn object_response_with_metadata(body: &str, version_id: &str, last_modified: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-amz-version-id: {version_id}\r\nlast-modified: {last_modified}\r\netag: \"current-etag\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn run_tfdiff(mock: &MockS3, args: &[&str]) -> std::process::Output {
    let cache = tempfile::tempdir().unwrap();
    run_tfdiff_with_cache(mock, args, cache.path())
}

fn run_tfdiff_with_cache(
    mock: &MockS3,
    args: &[&str],
    cache_root: &std::path::Path,
) -> std::process::Output {
    run_tfdiff_with_region(mock, args, cache_root, Some("us-east-1"))
}

fn run_tfdiff_with_region(
    mock: &MockS3,
    args: &[&str],
    cache_root: &std::path::Path,
    region: Option<&str>,
) -> std::process::Output {
    let mut command = Command::cargo_bin("tfdiff").unwrap();
    command
        .env_clear()
        .env("XDG_CACHE_HOME", cache_root)
        .env("AWS_ACCESS_KEY_ID", "offline-test-access")
        .env("AWS_SECRET_ACCESS_KEY", "offline-test-secret")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_PROFILE", "")
        .env("AWS_CONFIG_FILE", "/dev/null")
        .env("AWS_SHARED_CREDENTIALS_FILE", "/dev/null")
        .env("AWS_ENDPOINT_URL_S3", &mock.endpoint)
        .env("AWS_MAX_ATTEMPTS", "1")
        .env("TERM", "dumb")
        .env_remove("NO_COLOR")
        .env_remove("TFDIFF_NO_COLOR")
        .env_remove("CLICOLOR_FORCE");
    if let Some(region) = region {
        command.env("AWS_REGION", region);
    } else {
        command.env_remove("AWS_REGION");
    }
    command.args(args);
    command.output().unwrap()
}

const OLDER_PLAN: &str = r#"# test_resource.item will be created
+ resource "test_resource" "item" {
    + value = "one"
}
Plan: 1 to add, 0 to change, 0 to destroy."#;
const LATER_PLAN: &str = r#"# test_resource.item will be created
+ resource "test_resource" "item" {
    + value = "two"
}
Plan: 1 to add, 0 to change, 0 to destroy."#;

fn list_page(entries: &str, continuation: &str, truncated: bool) -> String {
    let truncated = if truncated { "true" } else { "false" };
    let (key_marker, version_marker) = if continuation.is_empty() {
        (String::new(), String::new())
    } else {
        (
            "<NextKeyMarker>reports%2Fplan.txt</NextKeyMarker>".to_string(),
            continuation.to_string(),
        )
    };
    response(
        "200 OK",
        &format!(
            "<ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name><Prefix>reports/plan.txt</Prefix><KeyMarker></KeyMarker><VersionIdMarker></VersionIdMarker><MaxKeys>1</MaxKeys><IsTruncated>{truncated}</IsTruncated>{entries}{key_marker}{version_marker}</ListVersionsResult>"
        ),
        "",
    )
}

fn history_page_for_key(key: &str, version_id: &str, last_modified: &str) -> String {
    response(
        "200 OK",
        &format!(
            "<ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name><Prefix>{key}</Prefix><MaxKeys>1</MaxKeys><IsTruncated>false</IsTruncated>{}</ListVersionsResult>",
            version_entry_for_key(key, version_id, last_modified, true)
        ),
        "",
    )
}

fn version_entry(version_id: &str, last_modified: &str, is_latest: bool) -> String {
    version_entry_for_key("reports/plan.txt", version_id, last_modified, is_latest)
}

fn version_entry_for_key(
    key: &str,
    version_id: &str,
    last_modified: &str,
    is_latest: bool,
) -> String {
    format!(
        "<Version><Key>{key}</Key><VersionId>{version_id}</VersionId><IsLatest>{is_latest}</IsLatest><LastModified>{last_modified}</LastModified><ETag>\"etag\"</ETag><Size>48</Size><StorageClass>STANDARD</StorageClass></Version>"
    )
}

fn delete_marker_entry(
    key: &str,
    version_id: &str,
    last_modified: &str,
    is_latest: bool,
) -> String {
    format!(
        "<DeleteMarker><Key>{key}</Key><VersionId>{version_id}</VersionId><IsLatest>{is_latest}</IsLatest><LastModified>{last_modified}</LastModified></DeleteMarker>"
    )
}

fn inventory_page_with_prefix(
    prefix: &str,
    entries: &str,
    is_truncated: bool,
    next_key: &str,
    next_version: &str,
) -> String {
    let truncated = if is_truncated { "true" } else { "false" };
    let markers = if is_truncated {
        format!("<NextKeyMarker>{next_key}</NextKeyMarker><NextVersionIdMarker>{next_version}</NextVersionIdMarker>")
    } else {
        String::new()
    };
    response(
        "200 OK",
        &format!("<ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name><Prefix>{prefix}</Prefix><KeyMarker></KeyMarker><VersionIdMarker></VersionIdMarker><MaxKeys>1000</MaxKeys><IsTruncated>{truncated}</IsTruncated>{entries}{markers}</ListVersionsResult>"),
        "",
    )
}

fn inventory_page(entries: &str, is_truncated: bool, next_key: &str, next_version: &str) -> String {
    inventory_page_with_prefix("dev/", entries, is_truncated, next_key, next_version)
}

fn state(serial: u64, value: &str) -> String {
    format!(
        r#"{{"version":4,"terraform_version":"1.9.0","serial":{serial},"lineage":"test","outputs":{{}},"resources":[{{"mode":"managed","type":"test_resource","name":"item","provider":"provider[\"registry.terraform.io/hashicorp/test\"]","instances":[{{"schema_version":0,"attributes":{{"id":"item","value":"{value}"}}}}]}}]}}"#
    )
}

fn vpc_endpoint_state(serial: u64, endpoint_id: &str, value: &str) -> String {
    format!(
        r#"{{"version":4,"terraform_version":"1.9.0","serial":{serial},"lineage":"test","outputs":{{}},"resources":[{{"mode":"managed","type":"aws_vpc_endpoint","name":"item","provider":"provider[\"registry.terraform.io/hashicorp/aws\"]","instances":[{{"schema_version":0,"attributes":{{"id":"{endpoint_id}","value":"{value}"}}}}]}}]}}"#
    )
}

fn state_with_deposed(serial: u64, current: &str, deposed: &str) -> String {
    serde_json::json!({
        "version": 4,
        "terraform_version": "1.9.0",
        "serial": serial,
        "lineage": "test",
        "outputs": {},
        "resources": [{
            "mode": "managed",
            "type": "test_resource",
            "name": "item",
            "provider": "provider[\"registry.terraform.io/hashicorp/test\"]",
            "instances": [
                {"schema_version": 0, "deposed": "deadbeef", "attributes": {"id": "old", "value": deposed}},
                {"schema_version": 0, "attributes": {"id": "item", "value": current}}
            ]
        }]
    })
    .to_string()
}

fn two_version_responses(
    newest_id: &str,
    newest_time: &str,
    newest_body: &str,
    older_id: &str,
    older_time: &str,
    older_body: &str,
) -> Vec<String> {
    vec![
        list_page(
            &format!(
                "{}{}",
                version_entry(newest_id, newest_time, true),
                version_entry(older_id, older_time, false)
            ),
            "",
            false,
        ),
        object_response(older_body),
        object_response(newest_body),
    ]
}

fn three_version_responses(
    newest: (&str, &str, &str),
    selected: (&str, &str, &str),
    predecessor: (&str, &str, &str),
) -> Vec<String> {
    vec![
        list_page(
            &version_entry(newest.0, newest.1, true),
            &format!(
                "<EncodingType>url</EncodingType><NextVersionIdMarker>{}</NextVersionIdMarker>",
                newest.0
            ),
            true,
        ),
        list_page(
            &version_entry(selected.0, selected.1, false),
            &format!(
                "<EncodingType>url</EncodingType><NextVersionIdMarker>{}</NextVersionIdMarker>",
                selected.0
            ),
            true,
        ),
        list_page(
            &version_entry(predecessor.0, predecessor.1, false),
            "",
            false,
        ),
        object_response(predecessor.2),
        object_response(selected.2),
    ]
}

#[test]
fn exact_object_time_bounds_select_pinned_versions_inclusively() {
    let mock = MockS3::start(vec![
        list_page(
            &version_entry("v3", "2026-01-03T00:00:00.000Z", true),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>v3</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("v2", "2026-01-02T00:00:00.000Z", false),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>v2</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("v1", "2026-01-01T00:00:00.000Z", false),
            "",
            false,
        ),
        object_response(OLDER_PLAN),
        object_response(LATER_PLAN),
    ]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/reports/plan.txt",
            "--since",
            "2026-01-01T00:00:00Z",
            "--until",
            "2026-01-02T00:00:00Z",
            "--format",
            "json",
        ],
    );
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["source_versions"]["left"]["version_id"], "v1");
    assert_eq!(report["source_versions"]["right"]["version_id"], "v2");
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=v1")));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=v2")));
    assert!(!requests
        .iter()
        .any(|request| request.contains("versionId=v3")));
}

#[test]
fn exact_object_time_bounds_render_compact_s3_terminal_provenance() {
    let mock = MockS3::start(vec![
        list_page(
            &version_entry("v2", "2026-01-02T00:00:00.000Z", true),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>v2</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("v1", "2026-01-01T00:00:00.000Z", false),
            "",
            false,
        ),
        object_response(&state(1, "older")),
        object_response(&state(2, "newer")),
    ]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/reports/plan.txt",
            "--since",
            "2026-01-01T00:00:00Z",
            "--until",
            "2026-01-02T00:00:00Z",
            "--color",
            "never",
        ],
    );
    mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("s3://bucket/reports/plan.txt"), "{stdout}");
    assert!(stdout.contains("- v1  (2026-01-01T00:00:00Z)"), "{stdout}");
    assert!(stdout.contains("+ v2  (2026-01-02T00:00:00Z)"), "{stdout}");
    assert!(!stdout.contains("Since"), "{stdout}");
    assert!(!stdout.contains("Until"), "{stdout}");
}

#[test]
fn since_only_uses_captured_now_and_until_only_uses_predecessor() {
    let since_only = MockS3::start(two_version_responses(
        "newer",
        "2026-01-02T00:00:00.000Z",
        LATER_PLAN,
        "older",
        "2026-01-01T00:00:00.000Z",
        OLDER_PLAN,
    ));
    let output = run_tfdiff(
        &since_only,
        &[
            "s3://bucket/reports/plan.txt",
            "--since",
            "2026-01-01T00:00:00Z",
        ],
    );
    let requests = since_only.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=older")));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=newer")));

    let until_only = MockS3::start(vec![
        list_page(
            &version_entry("v3", "2026-01-03T00:00:00.000Z", true),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>v3</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("v2", "2026-01-02T00:00:00.000Z", false),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>v2</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("v1", "2026-01-01T00:00:00.000Z", false),
            "",
            false,
        ),
        object_response(OLDER_PLAN),
        object_response(LATER_PLAN),
    ]);
    let output = run_tfdiff(
        &until_only,
        &[
            "s3://bucket/reports/plan.txt",
            "--until",
            "2026-01-02T12:00:00Z",
        ],
    );
    let requests = until_only.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=v1")));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=v2")));
    assert!(!requests
        .iter()
        .any(|request| request.contains("versionId=v3")));
}

#[test]
fn time_pair_rejects_same_selection_and_delete_markers() {
    let same = MockS3::start(vec![list_page(
        &format!(
            "{}{}",
            version_entry("v2", "2026-01-02T00:00:00.000Z", true),
            version_entry("v1", "2026-01-01T00:00:00.000Z", false)
        ),
        "",
        false,
    )]);
    let output = run_tfdiff(
        &same,
        &[
            "s3://bucket/reports/plan.txt",
            "--since",
            "2026-01-02T00:00:00Z",
            "--until",
            "2026-01-02T00:00:00Z",
        ],
    );
    let requests = same.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("same S3 history entry"));
    assert_eq!(requests.len(), 1);

    let marker = delete_marker_entry(
        "reports/plan.txt",
        "deleted",
        "2026-01-02T00:00:00.000Z",
        false,
    );
    let deleted = MockS3::start(vec![list_page(
        &format!(
            "{}{}{}",
            version_entry("v3", "2026-01-03T00:00:00.000Z", true),
            marker,
            version_entry("v1", "2026-01-01T00:00:00.000Z", false)
        ),
        "",
        false,
    )]);
    let output = run_tfdiff(
        &deleted,
        &[
            "s3://bucket/reports/plan.txt",
            "--since",
            "2026-01-02T00:00:00Z",
            "--until",
            "2026-01-03T00:00:00Z",
        ],
    );
    let requests = deleted.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("delete marker"));
    assert_eq!(requests.len(), 1, "delete markers are never fetched");
}

#[test]
fn explicit_version_is_compared_with_its_predecessor() {
    let mock = MockS3::start(three_version_responses(
        ("head", "2026-01-03T00:00:00.000Z", LATER_PLAN),
        ("release/one", "2026-01-02T00:00:00.000Z", LATER_PLAN),
        ("base", "2026-01-01T00:00:00.000Z", OLDER_PLAN),
    ));
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt@release%2Fone"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("s3://bucket/reports/plan.txt"), "{stdout}");
    assert!(
        stdout.contains("- base  (2026-01-01T00:00:00Z)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+ release/one  (2026-01-02T00:00:00Z)"),
        "{stdout}"
    );
    assert!(!stdout.contains("Previous Version"), "{stdout}");
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=release%2Fone")));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=base")));
}

#[test]
fn explicit_version_stops_listing_after_its_predecessor_is_found() {
    let mock = MockS3::start(vec![
        list_page(
            &version_entry("head", "2026-01-04T00:00:00.000Z", true),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>head</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("selected", "2026-01-03T00:00:00.000Z", false),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>selected</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("predecessor", "2026-01-02T00:00:00.000Z", false),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>predecessor</NextVersionIdMarker>",
            true,
        ),
        object_response(OLDER_PLAN),
        object_response(LATER_PLAN),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let output = run_tfdiff_with_cache(
        &mock,
        &["s3://bucket/reports/plan.txt@selected"],
        cache.path(),
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        requests.len(),
        5,
        "do not list history older than the predecessor"
    );
    assert!(requests[..3]
        .iter()
        .all(|request| request.starts_with("GET /bucket/?")));
    assert!(requests[3].contains("versionId=predecessor"));
    assert!(requests[4].contains("versionId=selected"));
    assert!(!requests
        .iter()
        .any(|request| request.contains("version-id-marker=predecessor")));
}

#[test]
fn current_s3_input_reports_metadata_from_object_response() {
    let current = state(2, "later");
    let mock = MockS3::start(vec![object_response_with_metadata(
        &current,
        "current-version",
        "Wed, 08 May 2026 12:04:53 GMT",
    )]);
    let older = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(older.path(), state(1, "older")).unwrap();
    let output = run_tfdiff(
        &mock,
        &[
            older.path().to_str().unwrap(),
            "s3://bucket/states/current.tfstate",
            "--format",
            "json",
        ],
    );
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 1);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["source_versions"]["left"], serde_json::Value::Null);
    assert_eq!(
        report["source_versions"]["right"]["version_id"],
        "current-version"
    );
    assert_eq!(
        report["source_versions"]["right"]["last_modified"],
        "2026-05-08T12:04:53Z"
    );
    assert_eq!(
        report["source_versions"]["right"]["etag"],
        "\"current-etag\""
    );
}

#[test]
fn current_s3_input_cache_hit_preserves_source_metadata() {
    let current = state(2, "later");
    let mock = MockS3::start(vec![
        object_response_with_metadata(&current, "current-version", "Wed, 08 May 2026 12:04:53 GMT"),
        response("304 Not Modified", "", ""),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let older = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(older.path(), state(1, "older")).unwrap();
    let args = [
        older.path().to_str().unwrap(),
        "s3://bucket/states/current.tfstate",
        "--format",
        "json",
    ];

    let first = run_tfdiff_with_cache(&mock, &args, cache.path());
    let second = run_tfdiff_with_cache(&mock, &args, cache.path());
    let requests = mock.finish();

    for output in [first, second] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            report["source_versions"]["right"]["version_id"],
            "current-version"
        );
        assert_eq!(
            report["source_versions"]["right"]["last_modified"],
            "2026-05-08T12:04:53Z"
        );
        assert_eq!(
            report["source_versions"]["right"]["etag"],
            "\"current-etag\""
        );
    }
    assert_eq!(requests.len(), 2);
    assert!(requests[1].to_ascii_lowercase().contains("if-none-match"));
}

#[test]
fn two_s3_positional_inputs_report_progress_only_to_stderr() {
    let mock = MockS3::start_routed(4, |request| {
        if request.starts_with("GET /bucket/?") && request.contains("prefix=states%2Folder.tfstate")
        {
            history_page_for_key(
                "states/older.tfstate",
                "older-version",
                "2026-01-01T00:00:00.000Z",
            )
        } else if request.starts_with("GET /bucket/?")
            && request.contains("prefix=states%2Flater.tfstate")
        {
            history_page_for_key(
                "states/later.tfstate",
                "later-version",
                "2026-01-02T00:00:00.000Z",
            )
        } else if request.contains("versionId=older-version") {
            object_response(&state(1, "older"))
        } else if request.contains("versionId=later-version") {
            object_response(&state(2, "later"))
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/states/older.tfstate@older-version",
            "s3://bucket/states/later.tfstate@later-version",
            "--format",
            "json",
        ],
    );
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 4);
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_ok());
    assert!(stderr.contains("reading 2 S3 object inputs"), "{stderr}");
    assert!(stderr.contains("reading S3 object 1/2"), "{stderr}");
    assert!(stderr.contains("reading S3 object 2/2"), "{stderr}");
    assert!(!stdout.contains("reading S3 object"));
}

#[test]
fn two_local_state_inputs_keep_generic_terminal_provenance() {
    let older = tempfile::NamedTempFile::new().unwrap();
    let later = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(older.path(), state(1, "before")).unwrap();
    std::fs::write(later.path(), state(2, "after")).unwrap();

    let mut command = Command::cargo_bin("tfdiff").unwrap();
    let output = command
        .env("TERM", "dumb")
        .args([
            older.path().to_str().unwrap(),
            later.path().to_str().unwrap(),
            "--color",
            "never",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(!stdout.contains("Previous Version"), "{stdout}");
    assert!(!stdout.contains("Selected Version"), "{stdout}");
    assert!(!stdout.contains("s3://"), "{stdout}");
    assert!(
        !stdout.contains("TERRAFORM STATE SNAPSHOT DIFF"),
        "{stdout}"
    );
    assert!(stdout.contains("value"), "{stdout}");
}

#[test]
fn implicit_head_compares_two_state_versions_and_reports_provenance() {
    let older = state(1, "before");
    let newer = state(2, "after");
    let mock = MockS3::start(two_version_responses(
        "newest",
        "2026-01-02T00:00:00.000Z",
        &newer,
        "older",
        "2026-01-01T00:00:00.000Z",
        &older,
    ));
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt", "--format", "json"]);
    let requests = mock.finish();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status: {}; stderr: {stderr}; stdout: {stdout}",
        output.status
    );
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(parsed["mode"], "StateDiff");
    assert_eq!(parsed["summary"]["change"], 1);
    assert_eq!(parsed["source_versions"]["left"]["version_id"], "older");
    assert_eq!(parsed["source_versions"]["right"]["version_id"], "newest");
    assert!(stderr.contains("older"));
    assert!(stderr.contains("newest"));
    assert!(requests[1].contains("versionId=older"));
    assert!(requests[2].contains("versionId=newest"));
}

#[test]
fn implicit_head_compares_terraform_text_plan_reports() {
    let mock = MockS3::start(two_version_responses(
        "newest",
        "2026-01-02T00:00:00.000Z",
        LATER_PLAN,
        "older",
        "2026-01-01T00:00:00.000Z",
        OLDER_PLAN,
    ));
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt"]);
    mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(!stdout.contains("TERRAFORM PLAN DIFF"), "{stdout}");
    assert!(stdout.contains("s3://bucket/reports/plan.txt"), "{stdout}");
    assert!(
        stdout.contains("- older  (2026-01-01T00:00:00Z)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+ newest  (2026-01-02T00:00:00Z)"),
        "{stdout}"
    );
    assert!(!stdout.contains("Previous Version"), "{stdout}");
    assert!(stdout.contains("Changed in report"), "{stdout}");
    assert!(stdout.contains("\"one\""), "{stdout}");
    assert!(stdout.contains("\"two\""), "{stdout}");
}

#[test]
fn explicit_hash_compares_selected_version_with_immediate_predecessor() {
    let mock = MockS3::start(three_version_responses(
        ("newest", "2026-01-03T00:00:00.000Z", LATER_PLAN),
        ("release-one", "2026-01-02T00:00:00.000Z", LATER_PLAN),
        ("base", "2026-01-01T00:00:00.000Z", OLDER_PLAN),
    ));
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt@release-one"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("s3://bucket/reports/plan.txt"), "{stdout}");
    assert!(
        stdout.contains("- base  (2026-01-01T00:00:00Z)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+ release-one  (2026-01-02T00:00:00Z)"),
        "{stdout}"
    );
    assert!(!stdout.contains("Previous Version"), "{stdout}");
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=release-one")));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=base")));
}

#[test]
fn head_offset_follows_s3_continuation_markers_and_pins_selected_version() {
    let mock = MockS3::start(three_version_responses(
        ("newest", "2026-01-03T00:00:00.000Z", LATER_PLAN),
        ("older", "2026-01-02T00:00:00.000Z", OLDER_PLAN),
        ("oldest", "2026-01-01T00:00:00.000Z", OLDER_PLAN),
    ));
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt@HEAD~1", "--summary"]);
    let requests = mock.finish();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests[0].starts_with("GET /bucket/?"));
    assert!(requests[0].contains("max-keys=1"));
    assert!(requests[0].contains("prefix=reports%2Fplan.txt"));
    assert!(requests[1].contains("key-marker=reports%2Fplan.txt"));
    assert!(requests[1].contains("version-id-marker=newest"));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=older")));
    assert!(requests
        .iter()
        .any(|request| request.contains("versionId=oldest")));
}

#[test]
fn explicit_version_reports_missing_and_oldest_versions_without_fetching_objects() {
    let missing = MockS3::start(vec![
        list_page(
            &version_entry("head", "2026-01-03T00:00:00.000Z", true),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>head</NextVersionIdMarker>",
            true,
        ),
        list_page(
            &version_entry("oldest", "2026-01-02T00:00:00.000Z", false),
            "",
            false,
        ),
    ]);
    let output = run_tfdiff(&missing, &["s3://bucket/reports/plan.txt@missing"]);
    let requests = missing.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("requested S3 version was not found"),
        "{stderr}"
    );
    assert_eq!(
        requests.len(),
        2,
        "missing versions should scan retained history"
    );
    assert!(requests
        .iter()
        .all(|request| request.starts_with("GET /bucket/?")));

    let oldest = MockS3::start(vec![list_page(
        &version_entry("oldest", "2026-01-01T00:00:00.000Z", false),
        "",
        false,
    )]);
    let output = run_tfdiff(&oldest, &["s3://bucket/reports/plan.txt@oldest"]);
    let requests = oldest.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("no immediately preceding retained version"),
        "{stderr}"
    );
    assert_eq!(
        requests.len(),
        1,
        "a missing predecessor must not fetch objects"
    );
}

#[test]
fn explicit_version_delete_markers_are_errors_and_are_not_skipped() {
    let marker = "<DeleteMarker><Key>reports/plan.txt</Key><VersionId>deleted</VersionId><IsLatest>false</IsLatest><LastModified>2026-01-02T00:00:00.000Z</LastModified></DeleteMarker>";
    let older = version_entry("older", "2026-01-01T00:00:00.000Z", false);
    let selected_marker = MockS3::start(vec![
        list_page(
            &version_entry("head", "2026-01-03T00:00:00.000Z", true),
            "<EncodingType>url</EncodingType><NextVersionIdMarker>head</NextVersionIdMarker>",
            true,
        ),
        list_page(&format!("{marker}{older}"), "", false),
    ]);
    let output = run_tfdiff(&selected_marker, &["s3://bucket/reports/plan.txt@deleted"]);
    let requests = selected_marker.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("selected version deleted is a delete marker"),
        "{stderr}"
    );
    assert_eq!(requests.len(), 2, "a delete marker must not be fetched");

    let selected = version_entry("selected", "2026-01-03T00:00:00.000Z", true);
    let preceding_marker = MockS3::start(vec![
        list_page(
            &selected,
            "<EncodingType>url</EncodingType><NextVersionIdMarker>selected</NextVersionIdMarker>",
            true,
        ),
        list_page(marker, "", false),
    ]);
    let output = run_tfdiff(
        &preceding_marker,
        &["s3://bucket/reports/plan.txt@selected"],
    );
    let requests = preceding_marker.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("immediately preceding S3 history entry is a delete marker"),
        "{stderr}"
    );
    assert_eq!(requests.len(), 2, "a delete marker must not be fetched");
}

#[test]
fn selected_delete_marker_is_an_error_and_is_not_skipped() {
    let marker = "<DeleteMarker><Key>reports/plan.txt</Key><VersionId>deleted</VersionId><IsLatest>true</IsLatest><LastModified>2026-01-02T00:00:00.000Z</LastModified></DeleteMarker>";
    let older = version_entry("older", "2026-01-01T00:00:00.000Z", false);
    let mock = MockS3::start(vec![list_page(&format!("{marker}{older}"), "", false)]);
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt@HEAD"]);
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("selected HEAD~0 is a delete marker"),
        "{stderr}"
    );
    assert_eq!(requests.len(), 1, "a delete marker must not be fetched");
}

#[test]
fn preceding_delete_marker_is_an_error_and_is_not_skipped() {
    let marker = "<DeleteMarker><Key>reports/plan.txt</Key><VersionId>deleted</VersionId><IsLatest>false</IsLatest><LastModified>2026-01-02T00:00:00.000Z</LastModified></DeleteMarker>";
    let selected = version_entry("selected", "2026-01-03T00:00:00.000Z", true);
    let mock = MockS3::start(vec![list_page(&format!("{selected}{marker}"), "", false)]);
    let output = run_tfdiff(&mock, &["s3://bucket/reports/plan.txt@HEAD"]);
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("immediately preceding S3 history entry is a delete marker"),
        "{stderr}"
    );
    assert_eq!(requests.len(), 1, "a delete marker must not be fetched");
}

#[test]
fn inventory_uri_lists_all_versions_and_delete_markers_for_matching_glob() {
    let entries = format!(
        "{}{}{}{}",
        version_entry_for_key(
            "dev/a/state.tfstate",
            "a2",
            "2026-01-03T00:00:00.000Z",
            true
        ),
        version_entry_for_key(
            "dev/a/state.tfstate",
            "a1",
            "2026-01-01T00:00:00.000Z",
            false
        ),
        version_entry_for_key(
            "dev/b/state.tfstate",
            "b1",
            "2026-01-02T00:00:00.000Z",
            true
        ),
        delete_marker_entry(
            "dev/c/state.tfstate",
            "c-delete",
            "2026-01-04T00:00:00.000Z",
            true
        ),
    );
    let mut responses = vec![inventory_page(&entries, false, "", "")];
    responses.extend([
        object_response(&state(1, "a-before")),
        object_response(&state(2, "a-after")),
    ]);
    let mock = MockS3::start(responses);
    let output = run_tfdiff(&mock, &["s3://bucket/dev/**/*.tfstate", "--format", "json"]);
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("prefix=dev%2F"));
    assert!(requests[0].contains("max-keys=1000"));
    assert!(requests[1].contains("/bucket/dev/a/state.tfstate"));
    assert!(requests[1].contains("versionId=a1"));
    assert!(requests[2].contains("/bucket/dev/a/state.tfstate"));
    assert!(requests[2].contains("versionId=a2"));
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["complete"], true);
    assert_eq!(report["counts"]["matching_keys"], 3);
    assert_eq!(report["counts"]["in_range_events"], 4);
    assert_eq!(report["counts"]["diffs"], 1);
    assert_eq!(report["counts"]["deletions"], 1);
    assert_eq!(report["events"][0]["kind"], "deletion");
    assert_eq!(report["events"][0]["version_id"], "c-delete");
    assert!(
        stderr.contains("tfdiff: S3 history: comparison dev/a/state.tfstate"),
        "{stderr}"
    );
    assert!(
        stderr.contains("tfdiff: S3 history: 1/1 comparisons processed"),
        "{stderr}"
    );
    assert!(!stdout.contains("S3 history progress"));
}

#[test]
fn inventory_comparisons_accept_complete_states_with_deposed_instances() {
    let entries = format!(
        "{}{}",
        version_entry_for_key("dev/a.tfstate", "a2", "2026-01-03T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-01-02T00:00:00.000Z", false),
    );
    let mock = MockS3::start(vec![
        inventory_page(&entries, false, "", ""),
        object_response(&state_with_deposed(1, "before", "retired-before")),
        object_response(&state_with_deposed(2, "after", "retired-after")),
    ]);
    let output = run_tfdiff(&mock, &["s3://bucket/dev/", "--format", "json"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 3);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["complete"], true);
    assert_eq!(report["counts"]["diffs"], 1);
    assert_eq!(report["counts"]["errors"], 0);
    assert!(report["events"][0]["plan"]["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["message"]
            .as_str()
            .unwrap()
            .contains("Ignored deposed instances")));
    assert!(!stdout.contains("retired-before"));
    assert!(!stdout.contains("retired-after"));
}

#[test]
fn inventory_comparisons_report_truncated_state_json_as_errors() {
    let entries = format!(
        "{}{}",
        version_entry_for_key("dev/a.tfstate", "a2", "2026-01-03T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-01-02T00:00:00.000Z", false),
    );
    let mock = MockS3::start(vec![
        inventory_page(&entries, false, "", ""),
        object_response(&state(1, "before")),
        object_response(
            r#"{"version":4,"terraform_version":"1.9.0","resources":[{"mode":"managed"}"#,
        ),
    ]);
    let output = run_tfdiff(&mock, &["s3://bucket/dev/", "--format", "json"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 3);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["complete"], false);
    assert_eq!(report["counts"]["diffs"], 0);
    assert_eq!(report["counts"]["errors"], 1);
    assert!(report["errors"][0]
        .as_str()
        .unwrap()
        .starts_with("dev/a.tfstate @a2:"));
    assert!(report["errors"][0]
        .as_str()
        .unwrap()
        .contains("malformed JSON-shaped S3 history entry"));
}

#[test]
fn inventory_pagination_failure_renders_partial_results_and_exits_nonzero() {
    let first = version_entry_for_key(
        "dev/a/state.tfstate",
        "a2",
        "2026-01-03T00:00:00.000000200Z",
        true,
    );
    let second = version_entry_for_key(
        "dev/a/state.tfstate",
        "a1",
        "2026-01-03T00:00:00.000000100Z",
        false,
    );
    let mock = MockS3::start(vec![
        inventory_page(
            &format!("{first}{second}"),
            true,
            "dev/a/state.tfstate",
            "a1",
        ),
        response(
            "503 Service Unavailable",
            "<Error><Code>SlowDown</Code></Error>",
            "",
        ),
        object_response(&state(1, "before")),
        object_response(&state(2, "after")),
    ]);
    let output = run_tfdiff(&mock, &["s3://bucket/dev/**/*.tfstate", "--format", "json"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["complete"], false);
    assert_eq!(report["counts"]["matching_keys"], 1);
    assert_eq!(report["counts"]["diffs"], 1);
    assert_eq!(report["counts"]["errors"], 1);
    assert!(report["errors"][0].as_str().unwrap().contains("SlowDown"));
    assert_eq!(requests.len(), 4);
    assert!(requests[1].contains("key-marker=dev%2Fa%2Fstate.tfstate"));
    assert!(requests[2].contains("versionId=a1"));
    assert!(requests[3].contains("versionId=a2"));
}

#[test]
fn inventory_comparisons_keep_out_of_order_failures_in_scheduled_order() {
    let entries = format!(
        "{}{}{}{}{}{}",
        version_entry_for_key("dev/a.tfstate", "a2", "2026-01-03T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-01-02T00:00:00.000Z", false),
        version_entry_for_key("dev/b.tfstate", "b2", "2026-01-03T00:00:00.000Z", true),
        version_entry_for_key("dev/b.tfstate", "b1", "2026-01-02T00:00:00.000Z", false),
        version_entry_for_key("dev/c.tfstate", "c2", "2026-01-03T00:00:00.000Z", true),
        version_entry_for_key("dev/c.tfstate", "c1", "2026-01-02T00:00:00.000Z", false),
    );
    let mock = MockS3::start_routed(7, move |request| {
        if request.starts_with("GET /bucket/?") {
            inventory_page(&entries, false, "", "")
        } else if request.contains("versionId=a1") {
            thread::sleep(std::time::Duration::from_millis(80));
            object_response(&state(1, "a-before"))
        } else if request.contains("versionId=a2") {
            object_response("{invalid-a")
        } else if request.contains("versionId=b1") {
            object_response(&state(1, "b-before"))
        } else if request.contains("versionId=b2") {
            object_response(&state(2, "b-after"))
        } else if request.contains("versionId=c1") {
            object_response(&state(1, "c-before"))
        } else if request.contains("versionId=c2") {
            object_response("{invalid-c")
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let output = run_tfdiff(&mock, &["s3://bucket/dev/", "--format", "json"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 7, "{requests:#?}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["counts"]["diffs"], 1);
    assert_eq!(report["counts"]["errors"], 2);
    let events = report["events"].as_array().unwrap();
    assert_eq!(events.len(), 4);
    let diffs: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "diff")
        .collect();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0]["key"], "dev/b.tfstate");
    let errors = report["errors"].as_array().unwrap();
    assert!(errors[0]
        .as_str()
        .unwrap()
        .starts_with("dev/a.tfstate @a2:"));
    assert!(errors[1]
        .as_str()
        .unwrap()
        .starts_with("dev/c.tfstate @c2:"));
    assert!(stderr.contains("3/3 comparisons processed"), "{stderr}");
    assert!(!stdout.contains("S3 history:"));
}

#[test]
fn inventory_batch_formats_render_aggregate_diffs_and_deletion_notes() {
    let entries = format!(
        "{}{}{}",
        version_entry_for_key(
            "dev/a/state.tfstate",
            "a2",
            "2026-01-03T00:00:00.000Z",
            true
        ),
        version_entry_for_key(
            "dev/a/state.tfstate",
            "a1",
            "2026-01-02T00:00:00.000Z",
            false
        ),
        delete_marker_entry(
            "dev/b/state.tfstate",
            "deleted",
            "2026-01-04T00:00:00.000Z",
            true
        ),
    );
    let deleted = version_entry_for_key(
        "dev/b/state.tfstate",
        "b1",
        "2026-01-01T00:00:00.000Z",
        false,
    );
    let html_mock = MockS3::start(vec![
        inventory_page(&format!("{entries}{deleted}"), false, "", ""),
        object_response(&state(1, "before")),
        object_response(&state(2, "after")),
    ]);
    let html_output = run_tfdiff(
        &html_mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--format",
            "html",
            "--full-inventory",
            "--skip-deleted",
        ],
    );
    html_mock.finish();
    let html = String::from_utf8_lossy(&html_output.stdout);
    let stderr = String::from_utf8_lossy(&html_output.stderr);
    assert!(html_output.status.success(), "{stderr}");
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("</main></body></html>"));
    assert_eq!(html.matches("<!DOCTYPE html>").count(), 1);
    assert!(html.contains("<details class=\"batch-event\""));
    assert!(html.contains("Current version <code>a2</code>"));
    assert!(html.contains("Compared with previous version <code>a1</code>"));
    assert!(html.contains("class=\"diff-container\""));
    assert!(html.contains("<h2>Full inventory</h2>"));
    assert!(html.contains("dev/b/state.tfstate</code></td>"));
    assert!(html.contains("Skipped 1 deletion events because --skip-deleted was set."));
    assert!(!html.contains("DELETED (deleted)"));

    let markdown_mock = MockS3::start(vec![
        inventory_page(&format!("{entries}{deleted}"), false, "", ""),
        object_response(&state(1, "before")),
        object_response(&state(2, "after")),
    ]);
    let markdown_output = run_tfdiff(
        &markdown_mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--format",
            "markdown",
            "--skip-deleted",
        ],
    );
    markdown_mock.finish();
    let markdown = String::from_utf8_lossy(&markdown_output.stdout);
    let stderr = String::from_utf8_lossy(&markdown_output.stderr);
    assert!(markdown_output.status.success(), "{stderr}");
    assert!(markdown.contains("# S3 State History"));
    assert!(markdown.contains("dev/a/state.tfstate"));
    assert!(markdown.contains("Skipped 1 deletion events because --skip-deleted was set."));

    let terminal_mock = MockS3::start(vec![
        inventory_page(&format!("{entries}{deleted}"), false, "", ""),
        object_response(&state(1, "before")),
        object_response(&state(2, "after")),
    ]);
    let terminal_output = run_tfdiff(
        &terminal_mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--format",
            "terminal",
            "--skip-deleted",
        ],
    );
    terminal_mock.finish();
    let terminal = String::from_utf8_lossy(&terminal_output.stdout);
    let stderr = String::from_utf8_lossy(&terminal_output.stderr);
    assert!(terminal_output.status.success(), "{stderr}");
    assert!(terminal.contains("S3 history: 2 matching keys"));
    assert!(terminal.contains("s3://bucket/dev/a/state.tfstate"));
    assert!(terminal.contains("- a1  (2026-01-02T00:00:00Z)"));
    assert!(terminal.contains("+ a2  (2026-01-03T00:00:00Z)"));
    assert!(!terminal.contains("version a2 (from a1)"));
    assert!(!terminal.contains("[2026-01-03T00:00:00Z] dev/a/state.tfstate"));
    assert!(terminal.contains("Skipped 1 deletion events because --skip-deleted was set."));
}

#[test]
fn inventory_diff_is_compact_by_default_and_pretty_restores_layout() {
    let entries = format!(
        "{}{}",
        version_entry_for_key("dev/a.tfstate", "a2", "2026-01-06T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-01-05T00:00:00.000Z", false),
    );
    let responses = || {
        vec![
            inventory_page(&entries, false, "", ""),
            object_response(&state(1, "before")),
            object_response(&state(2, "after")),
        ]
    };

    let compact_mock = MockS3::start(responses());
    let compact_output = run_tfdiff(&compact_mock, &["s3://bucket/dev/"]);
    compact_mock.finish();
    let compact = String::from_utf8_lossy(&compact_output.stdout);
    assert!(compact_output.status.success(), "{compact}");
    assert!(compact.contains("s3://bucket/dev/a.tfstate"), "{compact}");
    assert!(
        compact.contains("- a1  (2026-01-05T00:00:00Z)"),
        "{compact}"
    );
    assert!(
        compact.contains("+ a2  (2026-01-06T00:00:00Z)"),
        "{compact}"
    );
    assert!(
        !compact.contains("OBSERVED SNAPSHOT DIFFERENCES"),
        "{compact}"
    );
    assert!(!compact.contains("RESOURCES"), "{compact}");
    assert!(!compact.contains("──"), "{compact}");

    let pretty_mock = MockS3::start(responses());
    let pretty_output = run_tfdiff(&pretty_mock, &["s3://bucket/dev/", "--pretty"]);
    pretty_mock.finish();
    let pretty = String::from_utf8_lossy(&pretty_output.stdout);
    assert!(pretty_output.status.success(), "{pretty}");
    assert!(pretty.contains("RESOURCES"), "{pretty}");
    assert!(pretty.contains("- a1  (2026-01-05T00:00:00Z)"), "{pretty}");
}

#[test]
fn configured_region_unavailable_warning_is_printed_once_for_many_events() {
    let now = chrono::Utc::now();
    let newer = (now - chrono::Duration::days(2))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let older = (now - chrono::Duration::days(3))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let mut entries = String::new();
    for index in 1..=3 {
        entries.push_str(&version_entry_for_key(
            &format!("dev/{index}.tfstate"),
            &format!("v{index}b"),
            &newer,
            true,
        ));
        entries.push_str(&version_entry_for_key(
            &format!("dev/{index}.tfstate"),
            &format!("v{index}a"),
            &older,
            false,
        ));
    }
    let mut responses = vec![
        response(
            "301 Moved Permanently",
            "",
            "x-amz-bucket-region: eu-west-1\r\n",
        ),
        inventory_page(&entries, false, "", ""),
    ];
    for index in 1..=3 {
        let endpoint = format!("vpce-{index:017}");
        responses.push(object_response(&vpc_endpoint_state(1, &endpoint, "before")));
        responses.push(object_response(&vpc_endpoint_state(2, &endpoint, "after")));
    }
    let mock = MockS3::start(responses);
    let cache = tempfile::tempdir().unwrap();
    let output = run_tfdiff_with_region(
        &mock,
        &["s3://bucket/dev/", "--show-authors"],
        cache.path(),
        None,
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 8, "{requests:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warning_lines = stderr
        .lines()
        .filter(|line| line.contains("no configured AWS region is available"))
        .count();
    assert_eq!(warning_lines, 1, "{stderr}");
}

#[test]
fn quiet_inventory_suppresses_deletion_and_skipped_event_lines_but_keeps_diffs() {
    let entries = format!(
        "{}{}{}{}{}{}{}",
        version_entry_for_key("dev/a.tfstate", "a2", "2026-01-06T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-01-05T00:00:00.000Z", false),
        delete_marker_entry(
            "dev/b.tfstate",
            "b-delete",
            "2026-01-04T00:00:00.000Z",
            true
        ),
        version_entry_for_key("dev/b.tfstate", "b1", "2026-01-03T00:00:00.000Z", false),
        version_entry_for_key("dev/c.tfstate", "c2", "2026-01-06T00:00:00.000Z", true),
        delete_marker_entry(
            "dev/c.tfstate",
            "c-delete",
            "2026-01-05T00:00:00.000Z",
            false
        ),
        version_entry_for_key("dev/c.tfstate", "c1", "2026-01-04T00:00:00.000Z", false),
    );
    let responses = || {
        vec![
            inventory_page(&entries, false, "", ""),
            object_response(&state(1, "before")),
            object_response(&state(2, "after")),
        ]
    };

    let verbose_mock = MockS3::start(responses());
    let verbose_output = run_tfdiff(&verbose_mock, &["s3://bucket/dev/"]);
    assert_eq!(verbose_mock.finish().len(), 3);
    let verbose_stdout = String::from_utf8_lossy(&verbose_output.stdout);
    let verbose_stderr = String::from_utf8_lossy(&verbose_output.stderr);
    assert!(verbose_output.status.success(), "{verbose_stderr}");
    assert!(
        verbose_stdout.contains("DELETED (b-delete)"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout
            .contains("SKIPPED: immediately preceding S3 history event is a delete marker"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("s3://bucket/dev/a.tfstate"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("- a1  (2026-01-05T00:00:00Z)"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("+ a2  (2026-01-06T00:00:00Z)"),
        "{verbose_stdout}"
    );
    assert!(
        !verbose_stdout.contains("version a2 (from a1)"),
        "{verbose_stdout}"
    );

    let quiet_mock = MockS3::start(responses());
    let quiet_output = run_tfdiff(&quiet_mock, &["s3://bucket/dev/", "--quiet"]);
    assert_eq!(quiet_mock.finish().len(), 3);
    let quiet_stdout = String::from_utf8_lossy(&quiet_output.stdout);
    let quiet_stderr = String::from_utf8_lossy(&quiet_output.stderr);
    assert!(quiet_output.status.success(), "{quiet_stderr}");
    assert!(quiet_stdout.contains("S3 history:"), "{quiet_stdout}");
    assert!(!quiet_stdout.contains("DELETED ("), "{quiet_stdout}");
    assert!(!quiet_stdout.contains("SKIPPED:"), "{quiet_stdout}");
    assert!(
        quiet_stdout.contains("s3://bucket/dev/a.tfstate"),
        "{quiet_stdout}"
    );
    assert!(
        quiet_stdout.contains("- a1  (2026-01-05T00:00:00Z)"),
        "{quiet_stdout}"
    );
    assert!(
        quiet_stdout.contains("+ a2  (2026-01-06T00:00:00Z)"),
        "{quiet_stdout}"
    );
    assert!(
        !quiet_stdout.contains("version a2 (from a1)"),
        "{quiet_stdout}"
    );
    assert!(
        !quiet_stdout.contains("State diff actions describe observed snapshot differences"),
        "{quiet_stdout}"
    );
}

#[test]
fn author_attribution_routes_time_pair_summary_and_browser_output() {
    let mock = MockS3::start(vec![
        list_page(
            &format!(
                "{}{}",
                version_entry("v2", "2026-01-02T00:00:00.000Z", true),
                version_entry("v1", "2026-01-01T00:00:00.000Z", false)
            ),
            "",
            false,
        ),
        object_response(&vpc_endpoint_state(1, "vpce-0123456789abcdef0", "before")),
        object_response(&vpc_endpoint_state(2, "vpce-0123456789abcdef0", "after")),
    ]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/reports/plan.txt",
            "--since",
            "2026-01-01T00:00:00Z",
            "--until",
            "2026-01-02T00:00:00Z",
            "--format",
            "json",
            "--show-authors",
        ],
    );
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        stderr.matches("CloudTrail event history retains").count(),
        1
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["author_attribution"]["window_end"],
        "2026-01-02T00:00:00Z"
    );
    assert_eq!(report["author_attribution"]["status"], "outside_retention");

    let summary_mock = MockS3::start(two_version_responses(
        "v2",
        "2026-01-02T00:00:00.000Z",
        &vpc_endpoint_state(2, "vpce-0123456789abcdef0", "after"),
        "v1",
        "2026-01-01T00:00:00.000Z",
        &vpc_endpoint_state(1, "vpce-0123456789abcdef0", "before"),
    ));
    let output = run_tfdiff(
        &summary_mock,
        &[
            "s3://bucket/reports/plan.txt",
            "--summary",
            "--show-authors",
        ],
    );
    let requests = summary_mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        stderr.matches("CloudTrail event history retains").count(),
        1
    );
    assert!(stdout.contains("Observed differences:"), "{stdout}");
    assert!(stdout.contains("Observed resource actors:"), "{stdout}");

    let browser_mock = MockS3::start(two_version_responses(
        "v2",
        "2026-01-02T00:00:00.000Z",
        &vpc_endpoint_state(2, "vpce-0123456789abcdef0", "after"),
        "v1",
        "2026-01-01T00:00:00.000Z",
        &vpc_endpoint_state(1, "vpce-0123456789abcdef0", "before"),
    ));
    let output = run_tfdiff(
        &browser_mock,
        &[
            "s3://bucket/reports/plan.txt",
            "--browser",
            "--show-atomic-authors",
        ],
    );
    let requests = browser_mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        stderr.matches("CloudTrail event history retains").count(),
        1
    );
    let html_path = stderr
        .split("📄 HTML file saved to: ")
        .nth(1)
        .expect("failed browser launch should report its generated HTML path")
        .trim();
    let html = std::fs::read_to_string(html_path).unwrap();
    assert!(html.contains("CloudTrail attribution"), "{html}");
    assert!(html.contains("OutsideRetention"), "{html}");
    std::fs::remove_file(html_path).unwrap();
}

#[test]
fn inventory_author_attribution_includes_filtered_diff_plan_metadata_and_retention_notice() {
    let entries = format!(
        "{}{}",
        version_entry_for_key("dev/a.tfstate", "a2", "2026-01-03T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-01-02T00:00:00.000Z", false),
    );
    let mock = MockS3::start(vec![
        inventory_page(&entries, false, "", ""),
        object_response(&vpc_endpoint_state(1, "vpce-0123456789abcdef0", "before")),
        object_response(&vpc_endpoint_state(2, "vpce-0123456789abcdef0", "after")),
    ]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/",
            "--format",
            "json",
            "--show-authors",
            "--quiet",
        ],
    );
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        requests.len(),
        3,
        "author lookup must not alter S3 requests"
    );
    assert_eq!(
        stderr.matches("CloudTrail event history retains").count(),
        1
    );
    assert!(stdout.contains("\"author_attribution\""), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["events"][0]["plan"]["author_attribution"]["status"],
        "outside_retention"
    );
    assert_eq!(
        report["events"][0]["plan"]["author_attribution"]["window_end"],
        "2026-01-03T00:00:00Z"
    );
    assert_eq!(
        report["events"][0]["plan"]["resources"][0]["audit_attribution"]["status"],
        "outside_retention"
    );
}

#[test]
fn inventory_empty_results_are_valid_aggregate_reports() {
    let mock = MockS3::start(vec![inventory_page("", false, "", "")]);
    let output = run_tfdiff(&mock, &["s3://bucket/dev/**/*.tfstate", "--format", "json"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 1);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["complete"], true);
    assert_eq!(report["counts"]["matching_keys"], 0);
    assert_eq!(report["counts"]["in_range_events"], 0);
    assert!(report["events"].as_array().unwrap().is_empty());
}

#[test]
fn inventory_equal_timestamp_version_and_delete_marker_follow_is_latest() {
    let marker = delete_marker_entry(
        "dev/a/state.tfstate",
        "deleted",
        "2026-01-03T00:00:00.000000100Z",
        true,
    );
    let old = version_entry_for_key(
        "dev/a/state.tfstate",
        "older",
        "2026-01-03T00:00:00.000000100Z",
        false,
    );
    let mock = MockS3::start(vec![inventory_page(
        &format!("{old}{marker}"),
        false,
        "",
        "",
    )]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--format",
            "json",
            "--full-inventory",
        ],
    );
    mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["events"][0]["kind"], "deletion");
    assert_eq!(report["inventory"][0]["status"], "deleted");
    assert_eq!(
        report["inventory"][0]["last_modified"],
        "2026-01-03T00:00:00Z"
    );
}

#[test]
fn missing_region_is_discovered_from_head_bucket_and_used_for_signing() {
    let current = state(2, "later");
    let mock = MockS3::start(vec![
        response(
            "301 Moved Permanently",
            "",
            "x-amz-bucket-region: eu-west-1\r\n",
        ),
        object_response(&current),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let older = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(older.path(), state(1, "older")).unwrap();
    let output = run_tfdiff_with_region(
        &mock,
        &[
            older.path().to_str().unwrap(),
            "s3://bucket/states/current.tfstate",
        ],
        cache.path(),
        None,
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("HEAD /bucket/"), "{}", requests[0]);
    assert!(requests[1].starts_with("GET /bucket/states/current.tfstate"));
    assert!(
        requests[1].contains("/eu-west-1/s3/aws4_request"),
        "{}",
        requests[1]
    );
}

#[test]
fn aws_region_override_precedes_environment_and_skips_bucket_discovery() {
    let current = state(2, "later");
    let mock = MockS3::start(vec![object_response(&current)]);
    let cache = tempfile::tempdir().unwrap();
    let older = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(older.path(), state(1, "older")).unwrap();
    let output = run_tfdiff_with_region(
        &mock,
        &[
            older.path().to_str().unwrap(),
            "s3://bucket/states/current.tfstate",
            "--aws-region",
            "eu-central-1",
        ],
        cache.path(),
        Some("us-east-1"),
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 1, "configured region avoids HeadBucket");
    assert!(
        requests[0].contains("/eu-central-1/s3/aws4_request"),
        "{}",
        requests[0]
    );
}

#[test]
fn region_discovery_uses_a_distinct_signing_region_for_each_bucket() {
    let first = state(1, "first");
    let second = state(2, "second");
    let mock = MockS3::start_routed(6, move |request| {
        if request.starts_with("HEAD /bucket-a") || request.starts_with("HEAD /bucket-a/") {
            response(
                "301 Moved Permanently",
                "",
                "x-amz-bucket-region: eu-west-1\r\n",
            )
        } else if request.starts_with("HEAD /bucket-b") || request.starts_with("HEAD /bucket-b/") {
            response(
                "301 Moved Permanently",
                "",
                "x-amz-bucket-region: ap-southeast-2\r\n",
            )
        } else if request.starts_with("GET /bucket-a") || request.starts_with("GET /bucket-a/") {
            object_response(&first)
        } else if request.starts_with("GET /bucket-b") || request.starts_with("GET /bucket-b/") {
            object_response(&second)
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let output = run_tfdiff_with_region(
        &mock,
        &["s3://bucket-a/state.tfstate", "s3://bucket-b/state.tfstate"],
        cache.path(),
        None,
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().any(|request| {
        request.starts_with("GET /bucket-a/") && request.contains("/eu-west-1/s3/aws4_request")
    }));
    assert!(requests.iter().any(|request| {
        request.starts_with("GET /bucket-b/") && request.contains("/ap-southeast-2/s3/aws4_request")
    }));
}

#[test]
fn region_discovery_without_response_region_reports_configuration_guidance() {
    let mock = MockS3::start(vec![response("403 Forbidden", "", "")]);
    let cache = tempfile::tempdir().unwrap();
    let older = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(older.path(), state(1, "older")).unwrap();
    let output = run_tfdiff_with_region(
        &mock,
        &[
            older.path().to_str().unwrap(),
            "s3://bucket/states/current.tfstate",
        ],
        cache.path(),
        None,
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("could not determine the bucket region"),
        "{stderr}"
    );
    assert!(stderr.contains("--aws-region or AWS_REGION"), "{stderr}");
    assert_eq!(requests.len(), 1);
}

#[test]
fn access_denied_does_not_echo_service_body_or_credentials() {
    let body = "<Error><Code>AccessDenied</Code><Message>secret-object-payload</Message></Error>";
    let mock = MockS3::start(vec![response(
        "403 Forbidden",
        body,
        "x-amz-request-id: test-request\r\n",
    )]);
    let output = run_tfdiff(&mock, &["s3://bucket/private.txt"]);
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("AccessDenied"), "{stderr}");
    assert!(!stderr.contains("secret-object-payload"));
    assert!(!stderr.contains("offline-test-secret"));
}

fn versioning_response(status: Option<&str>) -> String {
    let status = status
        .map(|status| format!("<Status>{status}</Status>"))
        .unwrap_or_default();
    response(
        "200 OK",
        &format!(
            "<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{status}</VersioningConfiguration>"
        ),
        "",
    )
}

fn is_versioning_request(request: &str) -> bool {
    request.lines().next().is_some_and(|line| {
        line.starts_with("GET ") && line.contains("versioning") && !line.contains("versions")
    })
}

fn named_state(serial: u64, name: &str, value: &str) -> String {
    format!(
        r#"{{"version":4,"terraform_version":"1.9.0","serial":{serial},"lineage":"test","outputs":{{}},"resources":[{{"mode":"managed","type":"test_resource","name":"{name}","provider":"provider[\"registry.terraform.io/hashicorp/test\"]","instances":[{{"schema_version":0,"attributes":{{"id":"{name}","value":"{value}"}}}}]}}]}}"#
    )
}

fn compact_route(
    entries: String,
    objects: Vec<(&'static str, String)>,
) -> impl Fn(&str) -> String + Send + Sync + 'static {
    move |request| {
        if is_versioning_request(request) {
            versioning_response(Some("Enabled"))
        } else if let Some((_, body)) = objects
            .iter()
            .find(|(version, _)| request.contains(&format!("versionId={version}")))
        {
            object_response(body)
        } else if request.contains("versions") {
            inventory_page(&entries, false, "", "")
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    }
}

const COMPACT_WINDOW: [&str; 4] = [
    "--since",
    "2026-03-01T00:00:00Z",
    "--until",
    "2026-04-01T00:00:00Z",
];

fn compact_args<'a>(target: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![target, "--compact", "--utc"];
    args.extend(extra);
    args
}

#[test]
fn compact_rejects_suspended_bucket_after_one_request() {
    let mock = MockS3::start_routed(1, |_| versioning_response(Some("Suspended")));
    let output = run_tfdiff(
        &mock,
        &compact_args("s3://bucket/dev/**/*.tfstate", &["--format", "json"]),
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("status is Suspended"), "{stderr}");
    assert_eq!(requests.len(), 1);
    assert!(is_versioning_request(&requests[0]), "{}", requests[0]);
}

#[test]
fn compact_rejects_never_versioned_bucket_after_one_request() {
    let mock = MockS3::start_routed(1, |_| versioning_response(None));
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &[]));
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("versioning has never been enabled"),
        "{stderr}"
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn compact_versioning_permission_error_mentions_required_permission() {
    let mock = MockS3::start_routed(1, |_| {
        response(
            "403 Forbidden",
            "<Error><Code>AccessDenied</Code></Error>",
            "",
        )
    });
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &[]));
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("s3:GetBucketVersioning"), "{stderr}");
}

fn merge_fixture() -> (String, Vec<(&'static str, String)>) {
    let entries = [
        version_entry_for_key("dev/a.tfstate", "a3", "2026-03-20T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a2", "2026-03-10T00:00:00.000Z", false),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-02-10T00:00:00.000Z", false),
        version_entry_for_key("dev/b.tfstate", "b1", "2026-03-15T00:00:00.000Z", true),
        delete_marker_entry("dev/c.tfstate", "cm", "2026-03-15T00:00:00.000Z", true),
        version_entry_for_key("dev/c.tfstate", "c1", "2026-02-10T00:00:00.000Z", false),
        delete_marker_entry("dev/d.tfstate", "dm", "2026-03-06T00:00:00.000Z", true),
        version_entry_for_key("dev/d.tfstate", "d1", "2026-03-05T00:00:00.000Z", false),
        version_entry_for_key("dev/e.tfstate", "e1", "2026-02-10T00:00:00.000Z", true),
    ]
    .concat();
    let objects = vec![
        ("a1", named_state(1, "a", "old")),
        ("a2", named_state(2, "a", "middle")),
        ("a3", named_state(3, "a", "new")),
        ("b1", named_state(1, "b", "created")),
        ("c1", named_state(1, "c", "doomed")),
        ("d1", named_state(1, "d", "transient")),
        ("e1", named_state(1, "e", "same")),
    ];
    (entries, objects)
}

#[test]
fn compact_glob_merges_net_change_across_files() {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(6, compact_route(entries, objects));
    let mut extra = vec!["--format", "json"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &extra));
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(is_versioning_request(&requests[0]), "{}", requests[0]);
    let gets = |id: &str| {
        requests
            .iter()
            .filter(|request| request.contains(&format!("versionId={id}")))
            .count()
    };
    assert_eq!(gets("a1"), 1);
    assert_eq!(gets("a3"), 1);
    assert_eq!(gets("a2"), 0);
    assert_eq!(gets("b1"), 1);
    assert_eq!(gets("c1"), 1);
    assert_eq!(gets("d1"), 0);
    assert_eq!(gets("e1"), 0);

    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["complete"], true);
    assert_eq!(report["counts"]["matching_keys"], 5);
    assert_eq!(report["counts"]["unchanged_keys"], 1);
    assert_eq!(report["counts"]["absent_keys"], 1);
    let plan = &report["plan"];
    let resources = plan["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 3);
    assert!(resources
        .iter()
        .all(|resource| resource["source"].is_string()));
    assert_eq!(plan["summary"]["add"], 1);
    assert_eq!(plan["summary"]["change"], 1);
    assert_eq!(plan["summary"]["destroy"], 1);
    let sources = &plan["compact_sources"];
    assert_eq!(sources["create"][0]["key"], "dev/b.tfstate");
    assert_eq!(sources["update"][0]["key"], "dev/a.tfstate");
    assert_eq!(sources["update"][0]["from_version"], "a1");
    assert_eq!(sources["update"][0]["until_version"], "a3");
    assert_eq!(sources["destroy"][0]["key"], "dev/c.tfstate");
    assert_eq!(sources["destroy"][0]["deleted"], true);
}

#[test]
fn compact_since_warning_goes_to_stderr_only_with_since() {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(6, compact_route(entries.clone(), objects.clone()));
    let mut extra = vec!["--format", "json"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &extra));
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr.matches("Warning: ").count(), 1, "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let warning = report["since_warning"].as_str().unwrap();
    assert!(stderr.contains(warning), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Warning: "));

    let mock = MockS3::start_routed(5, compact_route(entries, objects));
    let output = run_tfdiff(
        &mock,
        &compact_args("s3://bucket/dev/**/*.tfstate", &["--format", "json"]),
    );
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("Warning: "), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["since_warning"].is_null());
}

#[test]
fn compact_without_since_treats_everything_as_created() {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(5, compact_route(entries, objects));
    let output = run_tfdiff(
        &mock,
        &compact_args(
            "s3://bucket/dev/**/*.tfstate",
            &["--until", "2026-04-01T00:00:00Z", "--format", "json"],
        ),
    );
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &report["plan"];
    assert_eq!(plan["summary"]["add"], 3);
    assert_eq!(plan["summary"]["change"], 0);
    assert_eq!(plan["summary"]["destroy"], 0);
    assert_eq!(
        plan["compact_sources"]["create"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn compact_same_address_in_two_files_yields_two_resources() {
    let entries = [
        version_entry_for_key("dev/x.tfstate", "x1", "2026-03-10T00:00:00.000Z", true),
        version_entry_for_key("dev/y.tfstate", "y1", "2026-03-11T00:00:00.000Z", true),
    ]
    .concat();
    let objects = vec![
        ("x1", named_state(1, "same", "one")),
        ("y1", named_state(1, "same", "two")),
    ];
    let mock = MockS3::start_routed(4, compact_route(entries, objects));
    let mut extra = vec!["--format", "json"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &extra));
    mock.finish();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let resources = report["plan"]["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);
    let mut sources: Vec<_> = resources
        .iter()
        .map(|resource| resource["source"].as_str().unwrap().to_string())
        .collect();
    sources.sort();
    assert_eq!(sources, ["dev/x.tfstate", "dev/y.tfstate"]);
}

#[test]
fn compact_skips_non_state_objects() {
    let entries = [
        version_entry_for_key("dev/a.tfstate", "a1", "2026-03-10T00:00:00.000Z", true),
        version_entry_for_key("dev/n.tfstate", "n1", "2026-03-11T00:00:00.000Z", true),
    ]
    .concat();
    let objects = vec![
        ("a1", named_state(1, "a", "one")),
        ("n1", "this is not terraform state".to_string()),
    ];
    let mock = MockS3::start_routed(4, compact_route(entries, objects));
    let mut extra = vec!["--format", "json"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &extra));
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["counts"]["skipped_non_state"], 1);
    let sources = serde_json::to_string(&report["plan"]["compact_sources"]).unwrap();
    assert!(sources.contains("dev/a.tfstate"));
    assert!(!sources.contains("dev/n.tfstate"));
}

#[test]
fn compact_exact_object_lists_by_key_and_ignores_siblings() {
    let entries = [
        version_entry_for_key("dev/a.tfstate", "a1", "2026-03-10T00:00:00.000Z", true),
        version_entry_for_key(
            "dev/a.tfstate.backup",
            "k1",
            "2026-03-11T00:00:00.000Z",
            true,
        ),
    ]
    .concat();
    let objects = vec![
        ("a1", named_state(1, "a", "one")),
        ("k1", named_state(1, "backup", "one")),
    ];
    let mock = MockS3::start_routed(3, compact_route(entries, objects));
    let mut extra = vec!["--format", "json"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/a.tfstate", &extra));
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(requests
        .iter()
        .any(|request| request.contains("prefix=dev%2Fa.tfstate")));
    assert!(!requests
        .iter()
        .any(|request| request.contains("versionId=k1")));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["counts"]["matching_keys"], 1);
    assert_eq!(report["plan"]["resources"].as_array().unwrap().len(), 1);
}

#[test]
fn compact_terminal_output_shows_header_and_sources() {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(6, compact_route(entries, objects));
    let output = run_tfdiff(
        &mock,
        &compact_args(
            "s3://bucket/dev/**/*.tfstate",
            &[
                "--since",
                "2026-03-01T00:00:00Z",
                "--until",
                "2026-04-01T00:00:00Z",
                "--color",
                "never",
            ],
        ),
    );
    mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("S3 compact:"), "{stdout}");
    assert!(stdout.contains("Create sources"), "{stdout}");
    assert!(stdout.contains("Source: dev/b.tfstate"), "{stdout}");
}

fn run_compact_terminal(extra: &[&str]) -> String {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(6, compact_route(entries, objects));
    let mut args = COMPACT_WINDOW.to_vec();
    args.extend(["--color", "never"]);
    args.extend(extra);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &args));
    mock.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn compact_show_atomic_sources_lists_versions_per_resource() {
    let stdout = run_compact_terminal(&["--show-atomic-sources"]);
    assert!(
        stdout.contains("Source: dev/a.tfstate  (a1 -> a3)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Source: dev/b.tfstate  ((none) -> b1)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Source: dev/c.tfstate  (c1 -> (none)) [deleted]"),
        "{stdout}"
    );
}

#[test]
fn compact_quiet_hides_source_files() {
    let stdout = run_compact_terminal(&["--quiet"]);
    for heading in ["Create sources", "Update sources", "Destroy sources"] {
        assert!(!stdout.contains(heading), "{stdout}");
    }
    assert!(!stdout.contains("Source:"), "{stdout}");
    assert!(!stdout.contains("dev/b.tfstate"), "{stdout}");
}

#[test]
fn compact_resource_filter_narrows_resources_and_sources() {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(6, compact_route(entries, objects));
    let mut extra = vec!["--json", "--filter-resource-name", "*.b"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(&mock, &compact_args("s3://bucket/dev/**/*.tfstate", &extra));
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &report["plan"];
    assert_eq!(plan["resources"].as_array().unwrap().len(), 1);
    assert_eq!(plan["resources"][0]["source"], "dev/b.tfstate");
    assert_eq!(plan["compact_sources"]["create"][0]["key"], "dev/b.tfstate");
    assert!(plan["compact_sources"]["update"]
        .as_array()
        .is_none_or(Vec::is_empty));
    assert!(plan["compact_sources"]["destroy"]
        .as_array()
        .is_none_or(Vec::is_empty));
}

#[test]
fn compact_regex_key_pattern_selects_matching_keys() {
    let (entries, objects) = merge_fixture();
    let mock = MockS3::start_routed(6, compact_route(entries, objects));
    let mut extra = vec!["--json", "--regex"];
    extra.extend(COMPACT_WINDOW);
    let output = run_tfdiff(
        &mock,
        &compact_args(r"s3://bucket/dev/[a-c]\.tfstate", &extra),
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(requests
        .iter()
        .any(|request| request.contains("prefix=dev%2F")));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["counts"]["matching_keys"], 3);
    assert_eq!(report["plan"]["resources"].as_array().unwrap().len(), 3);
}

#[test]
fn inventory_without_compact_never_requests_bucket_versioning() {
    let entries = [
        version_entry_for_key("dev/a.tfstate", "a2", "2026-03-20T00:00:00.000Z", true),
        version_entry_for_key("dev/a.tfstate", "a1", "2026-03-10T00:00:00.000Z", false),
    ]
    .concat();
    let mock = MockS3::start_routed(3, move |request| {
        assert!(!is_versioning_request(request), "{request}");
        if request.contains("versionId=a1") {
            object_response(&named_state(1, "a", "old"))
        } else if request.contains("versionId=a2") {
            object_response(&named_state(2, "a", "new"))
        } else {
            inventory_page(&entries, false, "", "")
        }
    });
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--utc",
            "--since",
            "2026-03-01T00:00:00Z",
            "--until",
            "2026-04-01T00:00:00Z",
            "--format",
            "json",
        ],
    );
    let requests = mock.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests
        .iter()
        .all(|request| !is_versioning_request(request)));
}

fn bucket_state(serial: u64, bucket: Option<(&str, &str)>) -> String {
    let resources = bucket
        .map(|(name, versioning)| {
            serde_json::json!([{
                "mode": "managed",
                "type": "aws_s3_bucket",
                "name": name,
                "provider": "provider[\"registry.terraform.io/hashicorp/aws\"]",
                "instances": [{"schema_version": 0, "attributes": {
                    "id": "acme-logs",
                    "arn": "arn:aws:s3:::acme-logs",
                    "versioning": versioning
                }}]
            }])
        })
        .unwrap_or_else(|| serde_json::json!([]));
    serde_json::json!({
        "version": 4,
        "terraform_version": "1.9.0",
        "serial": serial,
        "lineage": "test",
        "outputs": {},
        "resources": resources
    })
    .to_string()
}

fn data_bucket_state(serial: u64, bucket: Option<(&str, &str)>) -> String {
    let resources = bucket
        .map(|(name, versioning)| {
            serde_json::json!([{
                "mode": "data",
                "type": "aws_s3_bucket",
                "name": name,
                "provider": "provider[\"registry.terraform.io/hashicorp/aws\"]",
                "instances": [{"schema_version": 0, "attributes": {
                    "id": "acme-logs",
                    "arn": "arn:aws:s3:::acme-logs",
                    "versioning": versioning
                }}]
            }])
        })
        .unwrap_or_else(|| serde_json::json!([]));
    serde_json::json!({
        "version": 4,
        "terraform_version": "1.9.0",
        "serial": serial,
        "lineage": "test",
        "outputs": {},
        "resources": resources
    })
    .to_string()
}

fn migration_mock() -> MockS3 {
    migration_mock_with(bucket_state)
}

fn migration_mock_with(state: fn(u64, Option<(&str, &str)>) -> String) -> MockS3 {
    let entries = [
        (
            "dev/a/state.tfstate",
            "a2",
            "2026-01-03T00:00:00.000Z",
            true,
        ),
        (
            "dev/a/state.tfstate",
            "a1",
            "2026-01-01T00:00:00.000Z",
            false,
        ),
        (
            "dev/b/state.tfstate",
            "b2",
            "2026-01-04T00:00:00.000Z",
            true,
        ),
        (
            "dev/b/state.tfstate",
            "b1",
            "2026-01-02T00:00:00.000Z",
            false,
        ),
    ]
    .iter()
    .map(|(key, version, modified, latest)| version_entry_for_key(key, version, modified, *latest))
    .collect::<String>();
    let objects = [
        ("a1", state(1, Some(("logs", "Enabled")))),
        ("a2", state(2, None)),
        ("b1", state(1, None)),
        ("b2", state(2, Some(("acme_logs", "Suspended")))),
    ];
    MockS3::start_routed(5, move |request| {
        if let Some((_, body)) = objects
            .iter()
            .find(|(version, _)| request.contains(&format!("versionId={version}")))
        {
            object_response(body)
        } else if request.contains("versions") {
            inventory_page(&entries, false, "", "")
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    })
}

#[test]
fn inventory_detects_resources_moved_between_state_files() {
    let mock = migration_mock();
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--only-state-migrations",
            "--format",
            "json",
            "--utc",
        ],
    );
    mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["moved_resources"], 1, "{report}");
    let migration = &report["migrations"][0];
    assert_eq!(migration["type_name"], "aws_s3_bucket");
    assert_eq!(migration["identity_key"], "id");
    assert_eq!(migration["identity_value"], "acme-logs");
    assert_eq!(migration["from"]["key"], "dev/a/state.tfstate");
    assert_eq!(migration["from"]["address"], "aws_s3_bucket.logs");
    assert_eq!(migration["to"]["key"], "dev/b/state.tfstate");
    assert_eq!(migration["to"]["address"], "aws_s3_bucket.acme_logs");
    assert!(
        migration["changes"].to_string().contains("Suspended"),
        "{migration}"
    );

    let mock = migration_mock();
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--only-state-migrations",
            "--utc",
            "--color",
            "never",
        ],
    );
    mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("State migrations: 1 moved resource across 2 matching keys"),
        "{stdout}"
    );
    assert!(stdout.contains("MOVED"), "{stdout}");
    assert!(stdout.contains("aws_s3_bucket"), "{stdout}");
    assert!(stdout.contains("modified during move"), "{stdout}");

    let mock = migration_mock();
    let output = run_tfdiff(
        &mock,
        &["s3://bucket/dev/**/*.tfstate", "--utc", "--color", "never"],
    );
    mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("MOVED"), "{stdout}");
    assert!(
        stdout.contains("moved to s3://bucket/dev/b/state.tfstate"),
        "{stdout}"
    );
    assert!(
        stdout.contains("moved from s3://bucket/dev/a/state.tfstate"),
        "{stdout}"
    );
}

#[test]
fn only_state_migrations_honors_skip_data_and_only_data() {
    for (flag, moved) in [("--skip-data", 0), ("--only-data", 1)] {
        let mock = migration_mock_with(data_bucket_state);
        let output = run_tfdiff(
            &mock,
            &[
                "s3://bucket/dev/**/*.tfstate",
                "--only-state-migrations",
                flag,
                "--format",
                "json",
                "--utc",
            ],
        );
        mock.finish();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{flag}: {stderr}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["moved_resources"], moved, "{flag}: {report}");
    }

    let mock = migration_mock();
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--only-state-migrations",
            "--only-data",
            "--format",
            "json",
            "--utc",
        ],
    );
    mock.finish();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["moved_resources"], 0, "{report}");
}
