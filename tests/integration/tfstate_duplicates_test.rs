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

fn entry(key: &str, version_id: &str, last_modified: &str, is_latest: bool) -> String {
    format!(
        "<Version><Key>{key}</Key><VersionId>{version_id}</VersionId><IsLatest>{is_latest}</IsLatest><LastModified>{last_modified}</LastModified><ETag>\"etag-{version_id}\"</ETag><Size>48</Size><StorageClass>STANDARD</StorageClass></Version>"
    )
}

fn marker(key: &str, version_id: &str, last_modified: &str, is_latest: bool) -> String {
    format!(
        "<DeleteMarker><Key>{key}</Key><VersionId>{version_id}</VersionId><IsLatest>{is_latest}</IsLatest><LastModified>{last_modified}</LastModified></DeleteMarker>"
    )
}

fn listing(entries: &str) -> String {
    response(
        "200 OK",
        &format!("<ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name><Prefix>dev/</Prefix><KeyMarker></KeyMarker><VersionIdMarker></VersionIdMarker><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>{entries}</ListVersionsResult>"),
        "",
    )
}

fn state(lineage: &str, value: &str) -> String {
    format!(
        r#"{{"version":4,"serial":1,"lineage":"{lineage}","outputs":{{}},"resources":[{{"mode":"managed","type":"test_resource","name":"item","provider":"provider[\"registry.terraform.io/hashicorp/test\"]","instances":[{{"attributes":{{"id":"item","value":"{value}"}}}}]}}]}}"#
    )
}

/// The 16-character resource hash of every `state()` body (one `test_resource` with id `item`).
fn state_hash() -> String {
    tfdiff::cache::sha256_hex(b"test_resource|item")[..16].to_string()
}

fn duplicates_mock() -> MockS3 {
    let key = "dev/euc1/terraform.tfstate";
    let entries = format!(
        "{}{}{}{}{}",
        entry(key, "v4", "2026-01-05T00:00:00.000Z", true),
        entry(key, "v3", "2026-01-04T00:00:00.000Z", false),
        marker(key, "m1", "2026-01-03T00:00:00.000Z", false),
        entry(key, "v2", "2026-01-02T00:00:00.000Z", false),
        entry(key, "v1", "2026-01-01T00:00:00.000Z", false),
    );
    MockS3::start_routed(5, move |request| {
        if request.starts_with("GET /bucket/?") {
            listing(&entries)
        } else if request.contains("versionId=v4") || request.contains("versionId=v3") {
            object_response(&state("lineage-b", "same"))
        } else if request.contains("versionId=v2") {
            object_response(&state("lineage-b", "different"))
        } else if request.contains("versionId=v1") {
            object_response("{not json")
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    })
}

#[test]
fn prints_status_for_every_version() {
    let mock = duplicates_mock();
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/",
            "--show-tfstate-duplicates",
            "--verbose",
            "--show-datetime",
        ],
    );
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    let lines: Vec<_> = stdout.lines().collect();
    let hash = state_hash();
    assert_eq!(
        lines,
        [
            "standalone  (1 state file, 1 to keep)".to_string(),
            String::new(),
            format!("  ACTIVE  dev/euc1/terraform.tfstate  @v4 HEAD  1 res  #{hash}  2026-01-05T00:00:00Z"),
            format!("      s3://bucket/dev/euc1/terraform.tfstate @v4 #{hash} CURRENT 2026-01-05T00:00:00Z"),
            format!("      s3://bucket/dev/euc1/terraform.tfstate @v3 #{hash} KEPT 2026-01-04T00:00:00Z (no matching copy in another key)"),
            "      s3://bucket/dev/euc1/terraform.tfstate @m1 DELETED 2026-01-03T00:00:00Z".to_string(),
            format!("      s3://bucket/dev/euc1/terraform.tfstate @v2 #{hash} KEPT 2026-01-02T00:00:00Z (no matching copy in another key)"),
            "      s3://bucket/dev/euc1/terraform.tfstate @v1 KEPT 2026-01-01T00:00:00Z (unreadable state)".to_string(),
            String::new(),
            "removal candidates: none".to_string(),
        ]
    );
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
}

#[test]
fn skips_lock_files_without_fetching_them() {
    let key = "dev/euc1/terraform.tfstate";
    let lock = "dev/euc1/terraform.tfstate.tflock";
    let entries = format!(
        "{}{}",
        entry(lock, "l1", "2026-01-06T00:00:00.000Z", true),
        entry(key, "v1", "2026-01-05T00:00:00.000Z", true),
    );
    let mock = MockS3::start_routed(2, move |request| {
        if request.starts_with("GET /bucket/?") {
            listing(&entries)
        } else if request.contains("versionId=v1") {
            object_response(&state("lineage-b", "same"))
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let output = run_tfdiff(&mock, &["s3://bucket/dev/", "--show-tfstate-duplicates"]);
    let requests = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 2, "lock file must not be fetched");
    assert!(!stdout.contains("tflock"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "  ACTIVE  dev/euc1/terraform.tfstate  @v1 HEAD  1 res  #{}  ",
            state_hash()
        )),
        "{stdout}"
    );
}

#[test]
fn detects_version_moved_to_another_key() {
    let old = "old-directory-structure/terraform.tfstate";
    let new = "dev/acm/terraform.tfstate";
    let old_entries = format!(
        "{}{}",
        marker(old, "m1", "2026-01-03T00:00:00.000Z", true),
        entry(old, "v1", "2026-01-01T00:00:00.000Z", false),
    );
    let new_entries = entry(new, "w1", "2026-01-05T00:00:00.000Z", true);
    let mock = MockS3::start_routed(3, move |request| {
        if request.starts_with("GET /bucket/?") {
            listing(&format!("{old_entries}{new_entries}"))
        } else if request.contains("versionId=v1") || request.contains("versionId=w1") {
            object_response(&state("lineage-b", "same"))
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/**/*.tfstate",
            "--show-tfstate-duplicates",
            "--show-datetime",
        ],
    );
    let _ = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    let hash = state_hash();
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "acm  (2 state files, 1 to keep)".to_string(),
            String::new(),
            format!("  ACTIVE   dev/acm/terraform.tfstate                  @w1 HEAD  1 res  #{hash}  2026-01-05T00:00:00Z"),
            // The delete marker wins over OBSOLETE.
            format!("  DELETED  old-directory-structure/terraform.tfstate  @v1       1 res  #{hash}  2026-01-03T00:00:00Z  1/1 resources exist elsewhere"),
            String::new(),
            "  history".to_string(),
            "    2026-01-05T00:00:00Z  old-directory-structure  →  dev/acm  1 resource  (source had 1)".to_string(),
            String::new(),
            "removal candidates: none".to_string(),
        ]
    );
}

#[test]
fn full_hash_prints_the_whole_resource_hash() {
    let key = "dev/euc1/terraform.tfstate";
    let entries = entry(key, "v1", "2026-01-05T00:00:00.000Z", true);
    let mock = MockS3::start_routed(2, move |request| {
        if request.starts_with("GET /bucket/?") {
            listing(&entries)
        } else if request.contains("versionId=v1") {
            object_response(&state("lineage-b", "same"))
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/",
            "--show-tfstate-duplicates",
            "--full-hash",
        ],
    );
    let _ = mock.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    let full = tfdiff::cache::sha256_hex(b"test_resource|item");
    assert!(
        stdout.contains(&format!(
            "  ACTIVE  dev/euc1/terraform.tfstate  @v1 HEAD  1 res  #{full}  "
        )),
        "{stdout}"
    );
}

#[test]
fn full_hash_requires_show_tfstate_duplicates() {
    let mock = MockS3::start([]);
    let output = run_tfdiff(&mock, &["s3://bucket/dev/", "--full-hash"]);
    let _ = mock.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--show-tfstate-duplicates"));
}

#[test]
fn reports_progress_for_each_version_body_read() {
    let mock = duplicates_mock();
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/",
            "--show-tfstate-duplicates",
            "--unbuffer",
        ],
    );
    let requests = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 5, "delete markers are never fetched");
    for version in ["v4", "v3", "v2", "v1"] {
        assert!(
            stderr.contains(&format!("dev/euc1/terraform.tfstate @{version}")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("@m1"), "{stderr}");
    assert!(
        stderr.contains("read 4 version(s), classified 1 key(s)"),
        "{stderr}"
    );
}

#[test]
fn json_output_lists_statuses_and_edges() {
    let mock = duplicates_mock();
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/**/*.tfstate",
            "--show-tfstate-duplicates",
            "--format",
            "json",
        ],
    );
    let _ = mock.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON report");
    assert_eq!(report["bucket"], "bucket");
    assert_eq!(report["keys"][0]["status"], "ACTIVE");
    assert!(report["edges"].as_array().unwrap().is_empty());
    let versions = report["keys"][0]["versions"].as_array().unwrap();
    let statuses: Vec<_> = versions
        .iter()
        .map(|version| version["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["CURRENT", "KEPT", "DELETED", "KEPT", "KEPT"]);
    assert_eq!(versions[1]["version_id"], "v3");
}

#[test]
fn rejects_two_inputs() {
    let mock = MockS3::start([]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/",
            "s3://bucket/prod/",
            "--show-tfstate-duplicates",
        ],
    );
    let _ = mock.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exactly one S3 prefix or glob"));
}

#[test]
fn rejects_html_format() {
    let mock = MockS3::start([]);
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/dev/",
            "--show-tfstate-duplicates",
            "--format",
            "html",
        ],
    );
    let _ = mock.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--show-tfstate-duplicates supports --format terminal or json"));
}

#[test]
fn json_output_lists_obsolete_key_and_edge() {
    let old = "old/terraform.tfstate";
    let new = "dev/acm/terraform.tfstate";
    let entries = format!(
        "{}{}",
        entry(old, "v1", "2026-01-01T00:00:00.000Z", true),
        entry(new, "w1", "2026-01-05T00:00:00.000Z", true),
    );
    let mock = MockS3::start_routed(3, move |request| {
        if request.starts_with("GET /bucket/?") {
            listing(&entries)
        } else if request.contains("versionId=v1") {
            object_response(&state("lineage-a", "x"))
        } else if request.contains("versionId=w1") {
            object_response(&state("lineage-b", "y"))
        } else {
            response("404 Not Found", "<Error><Code>NoSuchKey</Code></Error>", "")
        }
    });
    let output = run_tfdiff(
        &mock,
        &[
            "s3://bucket/**/*.tfstate",
            "--show-tfstate-duplicates",
            "--format",
            "json",
        ],
    );
    let requests = mock.finish();
    assert!(output.status.success());
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let keys = report["keys"].as_array().unwrap();
    let old_node = keys.iter().find(|key| key["key"] == old).unwrap();
    assert_eq!(old_node["status"], "OBSOLETE");
    assert_eq!(old_node["detail"], "moved, same resource ids");
    let edges = report["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["from"]["key"], old);
    assert_eq!(edges[0]["to"]["version_id"], "w1");
    assert_eq!(edges[0]["resources"], 1);
}
