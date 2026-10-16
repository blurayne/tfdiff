# tfdiff - Terraform Plan and State Diff CLI Tool

A high-performance CLI tool written in Rust that transforms messy Terraform plan/apply outputs into beautiful, easy-to-understand visual diffs. Whether you prefer terminal output with rich colors or an interactive web browser experience, tfdiff makes reviewing infrastructure changes a breeze.

![Made with Rust](https://img.shields.io/badge/Made%20with-Rust-orange.svg)
![Tests](https://img.shields.io/badge/Tests-232%20passing-green.svg)
![Performance](https://img.shields.io/badge/Performance-%3C100ms-brightgreen.svg)

## Quick Start

```bash
# Install development dependencies
make deps

# Build the project
make build

# Run tests
make test

# See all available commands
make help
```

## ✨ Features

- 🎨 **Beautiful terminal output** with colors, symbols, and structured formatting
- 🌐 **Interactive browser view** with side-by-side diffs, filtering, and search
- 📄 **Multiple output formats**: Terminal, JSON, HTML, Markdown
- 🔍 **Smart parsing** that understands Terraform plan/apply outputs completely
- 🧹 **Intelligent input cleaning** removes ANSI codes, spinners, CDK prefixes
- ⚡ **Blazing fast performance** - processes 10MB+ outputs in <100ms
- 🔬 **Detailed resource diffs** showing exact attribute changes; in the terminal, the changed parts of an updated value are underlined, and resource addresses highlight dots, index numbers and quoted keys (for example `aws_ecr_repository.this["acme-app"]`)
- 🔒 **Secret masking by default** for passwords, tokens, secrets, private keys and AWS access key IDs (`--raw-secrets` to opt out)
- 🎯 **Action and data filtering** - focus on creates, updates, destroys, or data-resource changes
- 📟 **Pager and output controls** - optional pager, line-flushed output, and configurable terminal colors
- 🛡️ **Error resilient** - gracefully handles malformed or partial inputs
- ✅ **Well-tested** with 227 passing tests including property-based testing

## Installation

### From Source
```bash
# Clone and install
git clone https://github.com/yourusername/tfdiff.git
cd tfdiff
make install
```

### Using Cargo
```bash
cargo install tfdiff
```

### Using Docker
```bash
# Build Docker image
make docker-build

# Run with Docker
echo "Plan: 1 to add, 0 to change, 0 to destroy." | docker run --rm -i tfdiff:latest
```

## 🚀 Usage

### Basic Usage
```bash
# Pipe Terraform output directly
terraform plan | tfdiff

# Read from file
tfdiff plan.txt

# Different output formats
tfdiff plan.txt --format json
tfdiff plan.txt --format html > report.html
tfdiff plan.txt --format markdown > plan.md

# Shortcuts for --format json|markdown|html (mutually exclusive)
tfdiff plan.txt --json
tfdiff plan.txt --md > plan.md
tfdiff plan.txt --html > report.html
```

### State snapshot comparison

Compare two Terraform v4 state snapshots directly. The first path is the older (before) snapshot; the second is the later (after) snapshot. This mode reports observed resource additions, removals, and attribute updates; it does not refresh resources, infer replacements, or produce an applyable plan. Root outputs are currently excluded. Sensitive paths and secret-looking values are masked by default (see [Secret masking](#secret-masking)), but other state values can still be confidential: protect reports and exports like the original state files. Inputs may be local paths or read-only S3 object URIs; S3 inputs support Terraform v4 state snapshots and the existing Terraform text plan/apply output only, not JSON plans or binary `.tfplan` files.

```bash
# Terminal comparison and HTML report
tfdiff older.tfstate later.tfstate
tfdiff older.tfstate later.tfstate --format html > state-diff.html
tfdiff older.tfstate later.tfstate --browser

# Optional redacted, partial Terraform-show-like JSON export
tfdiff older.tfstate later.tfstate --generate-tfplan-file comparison.json
```

The export contains resource changes and sensitivity masks only; it is not a Terraform state file or binary plan and cannot be applied.

### Secret masking

Secrets are hidden by default in every output format (terminal, JSON, Markdown, HTML, `--browser`, `--generate-tfplan-file`, S3 inventories and `--compact`), for both text plans and state comparisons. A masked value is shown as `(sensitive value)`. Masked are:

- values Terraform itself marks as sensitive;
- attributes whose key contains `password`, `token`, `secret` or `private_key` (case-insensitive, at any nesting depth, for example `master_password`, `ACME_API_TOKEN` inside `environment.variables`, `client_secret`, `private_key_pem`); keys that only reference a secret and end in `_arn`, `_id` or `_name` (for example `secret_arn`, `kms_key_id`) stay visible, as do numbers, booleans and null under secret keys (for example `minimum_password_length`);
- string values that look like AWS access key IDs (`AKIA` followed by 16 uppercase letters or digits), under any key.

Use `--raw-secrets` to show all values in plain text; it disables every kind of masking, including Terraform's own sensitivity marks. Masking is a convenience, not a guarantee: other attributes can still contain confidential data, so treat reports like the original plans and state files.

```bash
tfdiff older.tfstate later.tfstate                 # secrets masked
tfdiff older.tfstate later.tfstate --raw-secrets   # secrets in plain text
```

### Read-only S3 inputs

S3 inputs use the built-in AWS Rust SDK and standard AWS configuration: credential provider chain (environment, shared credentials/config profiles, web identity, container/instance role where available), region, and SDK-supported endpoint settings. `--aws-region REGION` overrides the configured SDK region for S3 and audit requests. Without that option, tfdiff uses the normal AWS SDK region chain (including environment and profile configuration); only when no region is configured does it discover the target bucket's region from S3 response metadata. Bucket discovery is per bucket, so inputs spanning regions use region-specific clients. Configuration and credentials are initialized only when an S3 input is used. S3 operations are read-only and never mutate remote objects. By default, tfdiff caches ETag-identified object bodies locally under `$XDG_CACHE_HOME/tfdiff` or, when unset, `$HOME/.cache/tfdiff`. Cached Terraform state and plan bodies are sensitive data; protect the cache like the source objects and reports. S object listings are never cached, so each run fetches the current version list to find the latest versions; only object bodies keyed by version or ETag are cached. Use `--no-cache` or `TFDIFF_NO_CACHE=true` to disable body caching. Remove the `tfdiff` directory from the applicable cache location to clear cached data. When reading multiple S3 objects or S3 history comparisons, tfdiff reports progress to stderr (cache hits may avoid downloads) so structured output on stdout remains clean. Positional inputs accept any local/S3 combination, preserving the older/before then later/after order.

A single S3 URI compares object versions automatically: no selector (or `@HEAD`) selects the latest retained object version on the right and its immediately preceding retained version on the left. `@HEAD~N` selects the entry N positions behind the latest as the right side and compares it with the next older retained entry. An explicit `@VERSION_ID` selects that version on the right and compares it with its immediate predecessor. The selected and predecessor versions are fetched by their version IDs, so the comparison is pinned to the listed versions. This implicit version-diff behavior applies only to a single S3 argument; two-input comparisons preserve the supplied order.

For an exact S3 object, `--since` and `--until` instead select one version at or before each bound: the newest history entry whose `LastModified` is less than or equal to the bound. Both bounds select left and right versions respectively. `--since` alone compares that version with the newest version at or before one captured invocation time; `--until` alone compares the selected right version with its immediately older retained history entry. Bounds are inclusive. A pair that selects the same history entry, has no qualifying version, lacks the required predecessor, or selects a delete marker is an error. Both object reads are pinned to the listed version IDs. This mode is only available for one exact S3 object, not local files or two-input comparisons. The `--timezone`/`--utc` options also apply here. Compact durations such as `3d`, `12h`, `30m`, and `45s` mean that amount of time before the captured invocation time.

```bash
# Compare latest and preceding state versions
tfdiff 's3://bucket/dev/network/terraform.tfstate'

# Compare the selected version with its predecessor
tfdiff 's3://bucket/dev/network/terraform.tfstate@version-id'
tfdiff 's3://bucket/dev/network/terraform.tfstate@HEAD~2'

# Compare successive versions explicitly, preserving argument order
tfdiff 's3://bucket/dev/network/terraform.tfstate@version-id' 's3://bucket/dev/network/terraform.tfstate@HEAD'

# S3 Terraform text-plan report history can also be compared
tfdiff 's3://bucket/reports/plan.txt' --format html > report.html
```

The final raw `@` separates the object key from its selector. Percent-encode a literal `@` in the key as `%40`; key and explicit version-ID escapes are decoded once, and `+` remains a plus. Reserved selectors are exact uppercase `HEAD` and `HEAD~N`, where N is a nonnegative decimal integer (`HEAD~0` is equivalent to `HEAD`). Explicit version IDs are opaque, including a literal `null` version ID. Relative history requests require `s3:ListBucketVersions`; the identity also needs `s3:GetObjectVersion` for pinned versions. SSE-KMS objects additionally require appropriate KMS decrypt permissions. Missing, expired, or permanently deleted versions are unavailable, and delete markers count as history entries but cannot be read as object contents. A selected or immediately preceding delete marker causes an actionable error; markers are not silently skipped.

#### CloudTrail resource-actor evidence

Use `--show-authors` to summarize observed actors or `--show-atomic-authors` to attach observations to each changed managed resource. Attribution is available only when the later/right snapshot is an S3 object version with its S3 `LastModified` metadata (including S3 inventory comparisons). The inclusive lookup interval ends at that timestamp and extends backward by `--author-window` (default `5m`, maximum `24h`). It is a correlation heuristic, not an apply boundary: long applies, delayed writes, non-adjacent snapshots, and manual AWS actions can produce missing or unrelated matches. CloudTrail event history retains regional management events for 90 days; enabling either author option prints a notice to stderr. Matching events are evidence of AWS actions, not proof Terraform/OpenTofu caused them. Current caller identity and state `terraform_version` are never presented as historical attribution. When an observation carries an email-shaped principal (from `sourceIdentity`, the session label, or `username`), the resource header shows it as `@email` even without `--show-full-attribution`; only the email is shown because CloudTrail does not provide a display name.

Global AWS services record their management events in `us-east-1`, so global-service resources are looked up there first. Only when that lookup yields no matching event does tfdiff query the configured region(s) (`--author-region`, or the AWS region). Each region is queried at most once per run. Currently supported resource types are `aws_vpc_endpoint` (regional) and `aws_iam_role_policy_attachment` (global, matched by `AttachRolePolicy`/`DetachRolePolicy` on `roleName` and `policyArn`). The global-first routing applies to `aws_iam_*`, `aws_cloudfront_*`, `aws_route53_*`, and `aws_organizations_*` resource types; only the IAM and CloudFront entries are verified against the documented CloudTrail behavior, and Route 53 and Organizations are unverified and may be recorded elsewhere. Override the global lookup region per service with `--aws-iam-region REGION` (for `aws_iam_*`) and `--aws-global-region REGION` (for the other global types); both default to `us-east-1`.

```bash
# Compare local older state to a later S3 state, then summarize observed AWS actors
tfdiff older.tfstate 's3://bucket/dev/terraform.tfstate@HEAD' --show-authors

# Also annotate individual resource changes and query an additional explicit region
tfdiff 's3://bucket/dev/terraform.tfstate@version-id' 's3://bucket/dev/terraform.tfstate@HEAD' \
  --show-authors --show-atomic-authors --author-region eu-central-1 --author-window 15m

# Look up IAM events in a different global-service region before the configured regions
tfdiff older.tfstate 's3://bucket/dev/terraform.tfstate@HEAD' --show-authors --aws-iam-region us-west-2
```

#### S3 prefix and glob inventory

Pass a prefix (with a trailing slash) or a glob to report every matching Terraform state change across object history. A bare path without a slash is tried as an exact object first; only when no exact history exists is it treated as a folder prefix with a slash boundary. Globs are matched against decoded S3 keys; `*` and `?` do not cross `/`, while `**` spans directories. Brace alternatives such as `{dev,prod}` are supported. Percent-encode literal glob characters, for example `%2A` for a literal `*`; encode a literal `@` as `%40`. Batch inputs do not accept version selectors.

```bash
# Every state history change beneath a prefix
tfdiff 's3://bucket/dev/'

# Recursive Terraform state glob with a date window
tfdiff 's3://bucket/{dev,prod}/**/*.tfstate' --since '5 days ago' --until today

# A fixed time window in a named timezone, with all matched keys listed
tfdiff 's3://bucket/dev/' --since '2026-10-10 12:00 America/Los_Angeles' --until '2026-10-12T22:30-07:00' --full-inventory

# Oldest first; omit deletion events and summarize them once at the end
tfdiff 's3://bucket/dev/' --sort-order asc --skip-deleted --format html > history.html
```

The date bounds are inclusive and apply to each version/delete-marker event's own `LastModified`; every matching in-range event is emitted. A readable object version is compared only with the immediately preceding history entry for that key, even when that predecessor is outside the selected window. `today` means the current local calendar day by default. Use `--utc` or `--timezone ZONE` (IANA zone names and numeric offsets are supported) to select a default timezone; an explicit offset or IANA timezone suffix on a timestamp takes precedence. Supported relative phrases are `today`, `now`, `N seconds|minutes|hours|days|weeks ago`, and compact positive durations such as `3d`, `12h`, `30m`, or `45s` (relative to one captured invocation time). Ambiguous or nonexistent wall times at DST transitions are rejected; date-only `--since` and `--until` cover the full selected calendar day.

Inventory sorts newest first by default; `--sort-order asc` reverses event chronology. Deletion events are included by default; `--skip-deleted` omits them and prints one aggregate note. `--full-inventory` adds every matching key's latest modification, presence/deletion status, and number of in-range events without fetching unrelated object bodies. Contents—not file extensions—identify Terraform state snapshots. Supported Terraform v4 state JSON versions are compared; unrelated objects are skipped. Malformed or unsupported Terraform state data and read failures are reported while processing continues for independent events. The report marks incomplete/error results and the command exits nonzero if inventory processing has errors; partial results are still emitted. JSON output is one aggregate JSON object on stdout and includes completeness, counts, events, optional inventory, and errors.

Terminal output for a single-object S3 history diff and each inventory state diff is compact by default: it starts with the sanitized object URI, followed by `- previous-version-id (previous-timestamp)` and `+ selected-version-id (selected-timestamp)`. The URI is bold bright white when terminal color is enabled. The boxed title, section headings, and divider lines are omitted; `--pretty` restores the full layout. Create, Update, and Destroy resource headers show the relevant state date in grey as a relative age (for example `4 hours ago`); `--show-datetime` shows the full RFC3339 timestamp instead. The `-`/`+` version lines always keep full timestamps. Inventory diff events omit their redundant timestamp/key/version banner; deletion, skipped, and error details remain visible. Local and mixed-input comparisons retain their generic source-version labels. JSON output stays valid on stdout; its source-version metadata is written to stderr. When more than one S3 history object body will be read for comparisons, tfdiff reports the number of object downloads to stderr; JSON and other report data stay on stdout. S3 history comparisons run concurrently with a bounded worker count; output events and errors are presented in deterministic history order. With `--unbuffer`, progress is written as line-based stderr updates instead of a redraw-style progress bar. History selectors are read-only but not atomic: concurrent writes or version removal can change the history between listing and download. In an unversioned or versioning-suspended bucket, S3's `null` version may be overwritten, so it is not an immutable snapshot. S3 reports, cached bodies, and state values may contain confidential information; handle cache files, stdout, and generated reports accordingly. State exports remain local-only.

#### Resources moved between state files

In the per-version inventory mode (without `--compact`), tfdiff tracks every created and destroyed resource by its resource type plus identity attribute (`id`, then `arn`, then `name`; empty, masked, and `(known after apply)` values are ignored). When a resource is destroyed in one state file and a resource of the same type with the same identity is created in a different state file, the pair is reported as a move: each matching CREATE or DESTROY line gets a `MOVED` badge with a yellow background and a note such as `moved to s3://bucket/dev/b.tfstate (aws_s3_bucket.logs)` or `moved from …`, plus the number of attributes that changed during the move. Each create is paired with the nearest destroy in time; one destroy pairs with at most one create. The `--filter-resource-name`/`--filter-resource-arn`/`--filter-resource-id` filters narrow move detection too, while action filters (`--filter`) do not hide moves.

Add `--only-state-migrations` to show nothing but the moves, sorted by resource type and identity, so you can follow each resource from its old state file to its new one:

```bash
tfdiff 's3://bucket/{dev,prod}/**/*.tfstate' --since 30d --only-state-migrations
tfdiff 's3://bucket/dev/' --only-state-migrations --format json
```

```text
State migrations: 1 moved resource across 2 matching keys

 MOVED  aws_s3_bucket  id: acme-logs
  FROM s3://bucket/dev/a/state.tfstate  aws_s3_bucket.logs  2026-01-03T00:00:00Z (a2)
  TO   s3://bucket/dev/b/state.tfstate  aws_s3_bucket.acme_logs  2026-01-04T00:00:00Z (b2)
  modified during move, 1 attribute changed:
  ~ versioning
    - "Enabled"
    + "Suspended"
```

Unchanged moves read `unchanged during move`. Markdown and HTML show a From/To table plus the attribute changes per move; JSON contains `source`, `complete`, `moved_resources`, `migrations` (each with `type_name`, `identity_key`, `identity_value`, `from`/`to` with `key`, `uri`, `address`, `timestamp`, `version_id`, and `changes`), and `errors`; `--summary` prints only the header line with the move count. `--only-state-migrations` requires exactly one S3 prefix or glob input and cannot be combined with `--compact`. The normal inventory JSON also carries a `migrations` array when moves were detected. Moves are detected only within the selected time window, so a destroy and its matching create must both be in range.

#### S3 compact net change

Add `--compact` to an S3 prefix, glob, or exact object input to get one merged diff of the net change across all matched state files between `--since` and `--until`, instead of one diff per version pair. An exact object is treated as a glob that matches a single key.

Omitting `--since` starts from the beginning of history, so every file that exists at `--until` counts as created. With `--since`, tfdiff prints a warning on stderr (and includes it in every output format) because the results may be incomplete: earlier history is collapsed into each file's starting state.

```bash
# Net change across a glob over the last 7 days
tfdiff 's3://bucket/{dev,prod}/**/*.tfstate' --compact --since '7 days ago'

# Net change beneath a prefix between two points in time, as an HTML report
tfdiff 's3://bucket/dev/' --compact --since '2026-10-01' --until '2026-10-08' --format html > net-change.html

# A single exact state object
tfdiff 's3://bucket/dev/app.tfstate' --compact --since 3d
```

For every matched key, the "from" side is the newest version or delete marker with `LastModified` at or before `--since`, even if it was written long before the window; with no `--since` the from side is absent, so everything present at `--until` shows as created. The "until" side is the newest version or delete marker at or before `--until` (default: now, captured once per invocation). A delete marker counts as the object being absent. Intermediate versions are never downloaded, only the from and until bodies are read.

A key that was present at `--since` and is a delete marker at `--until` is reported as a destroy and listed with `[deleted]`. Keys that are absent on both sides (for example created and deleted inside the window) and keys whose from and until are the same version are omitted from the diff; they are only reflected in the header counts.

The result is a single diff in which every resource carries the key of the file it came from. The same address in two files appears as two resources with different sources, and moving a resource between stacks shows as a destroy in one file and a create in the other. Terminal, HTML, and Markdown output print a `Source: <key>` line per resource, and the files contributing to each action are listed under `Create sources`, `Update sources`, and `Destroy sources` as `key  (from -> until)`. Terminal output starts with a header:

```text
S3 compact: 12 matching keys | 3 changed | 1 created | 1 deleted | 6 unchanged | 1 non-state skipped | 0 errors | since 2026-10-01T00:00:00Z | until 2026-10-08T00:00:00Z
```

The header shows `since beginning` when `--since` is omitted and ends with ` | INCOMPLETE` when listing or reads failed. JSON output has `source` on each resource and `compact_sources` grouped by action:

```json
{
  "resources": [
    { "id": "aws_s3_bucket.logs", "action": "create", "source": "dev/b.tfstate" }
  ],
  "compact_sources": {
    "create": [
      { "key": "dev/b.tfstate", "from_version": null, "until_version": "v7", "deleted": false }
    ],
    "update": [
      { "key": "dev/a.tfstate", "from_version": "v1", "until_version": "v3", "deleted": false }
    ],
    "destroy": [
      { "key": "dev/c.tfstate", "from_version": "v2", "until_version": null, "deleted": true }
    ]
  }
}
```

Add `--show-atomic-sources` (requires `--compact`) to show the exact source versions involved in every Create, Update, and Destroy: in terminal, HTML, and Markdown output each resource's `Source:` line then includes the from and until version IDs, with `(none)` for an absent side and ` [deleted]` when the file was removed by a delete marker. JSON keeps the plain key in `source`; the version details are already in `compact_sources`. Because this output can be large, combine it with the resource filters described under [Advanced Usage](#advanced-usage).

```text
Source: dev/a.tfstate  (a1 -> a3)
Source: dev/b.tfstate  ((none) -> b1)
Source: dev/c.tfstate  (c1 -> (none)) [deleted]
```

```bash
# Only S3 buckets, with the versions each change came from
tfdiff 's3://bucket/{dev,prod}/**/*.tfstate' --compact --since 7d --show-atomic-sources --filter-resource-name 'aws_s3_*'
```

With `--compact`, `--quiet` hides all source files in every output format: the per-action source lists and every per-resource `Source:` line are omitted.

Only complete objects are considered: `ListObjectVersions` returns completed versions and delete markers, while in-progress or aborted multipart uploads are never listed. Objects whose contents are not supported Terraform v4 state are counted as non-state skipped and never appear as sources. Read failures are reported and the command exits nonzero after rendering partial results.

`--compact` requires S3 bucket versioning to be `Enabled` and checks it before listing anything; a suspended or never-enabled bucket is a hard error (`--compact requires S3 bucket versioning to be Enabled; bucket versioning status is Suspended`, or `… versioning has never been enabled on this bucket`). This needs the `s3:GetBucketVersioning` permission in addition to the list and read permissions. Validation errors occur before any AWS call:

- `--compact requires exactly one S3 prefix, glob, or exact object input` (local files, stdin, or two paths).
- `--compact cannot be combined with an explicit S3 version selector; use --since/--until`
- `--compact cannot be combined with --skip-deleted, --full-inventory, or --sort-order`
- `--compact does not support --show-authors or --show-atomic-authors`
- `--compact cannot be combined with --generate-tfplan-file`

### 🌐 Interactive Browser Mode
The browser mode opens a clean HTML view in your default browser:

```bash
# Open plan in browser
tfdiff plan.txt --browser

# Also works with piped input
terraform plan | tfdiff --browser
```

**Browser Features:**
- 🎯 **Side-by-side diffs** showing old and new values
- 📱 **Responsive layout** works on mobile and desktop

### Advanced Usage
```bash
# Filter by action type in terminal
tfdiff plan.txt --filter create,update

# Omit data resources or show only data resources (including module-qualified addresses)
tfdiff plan.txt --skip-data
tfdiff plan.txt --only-data

# Hide the extra changed-resource identity label without removing JSON attributes
tfdiff older.tfstate later.tfstate --skip-ids --format json

# Sort resources by action (create, update, destroy, read, noop), then type, name, and ID
tfdiff plan.txt --sort-by action

# Page terminal output (only when stdout is a TTY)
tfdiff plan.txt --pager
tfdiff plan.txt -p

# Bypass a pager and flush each already-rendered output line
# (This is line-flushed output, not streaming parsing or formatting.)
tfdiff plan.txt --pager --unbuffer

# Choose terminal colors explicitly
tfdiff plan.txt --color always
tfdiff plan.txt --color never

# Verbose output with extra details
tfdiff plan.txt --verbose

# Summary only mode
tfdiff plan.txt --summary

# Quiet mode - minimal output (with --compact this also hides source files)
tfdiff plan.txt --quiet

# Filter resources by address/type/name, ARN, or ID (glob by default)
tfdiff plan.txt --filter-resource-name 'aws_*'
tfdiff plan.txt --filter-resource-arn 'arn:aws:iam::*:role/app-*'
tfdiff plan.txt --filter-resource-id 'vpce-*'

# Use regular expressions instead of globs
tfdiff plan.txt --regex --filter-resource-name 'aws_(iam|kms)_.*'
tfdiff 's3://bucket/dev/[a-c]\.tfstate' --compact --regex
```

#### Resource filters and pattern mode

`--filter-resource-name` matches a resource's full address, its type, or its name. `--filter-resource-arn` and `--filter-resource-id` match the `arn`/`id` attribute or the before/after value of a top-level, non-sensitive `arn`/`id` change. Each flag can be repeated: patterns of the same flag are OR-ed, and different flags are AND-ed. Filters apply to managed and data resources and work with plans, state comparisons, S3 inventories, and `--compact` (where the per-action source lists are narrowed to the remaining resources).

All patterns are globs by default (`--glob`); `--regex` switches every pattern to regular expressions, including S3 key patterns in `s3://` inputs (`--glob` and `--regex` are mutually exclusive). In glob mode `*` matches any characters, including `/` and `.`, for resource filters, while S3 key globs keep their path semantics described above. Patterns always match the whole value: a regex is implicitly anchored as `^(?:pattern)$`. An invalid pattern is an error before any input is read, for example `invalid regex pattern 'aws_(': …`.

### Real-world Examples
```bash
# Review a large infrastructure change in browser
terraform plan -out=plan.tfplan
terraform show plan.tfplan | tfdiff --browser

# Generate HTML report for team review
terraform plan | tfdiff --format html > infrastructure-changes.html

# Quick terminal summary of changes
terraform plan | tfdiff --summary

# Export detailed JSON for automation
terraform plan | tfdiff --format json > changes.json
```

## Development

### Prerequisites
- Rust 1.94.1+
- Make
- Docker (optional)

### Development Workflow

```bash
# Set up development environment
make deps

# Development cycle
make dev          # Start file watching for development
make test-watch   # Run tests in watch mode

# Code quality
make qa           # Run all quality checks
make lint         # Run linting
make fmt          # Format code

# Performance
make bench        # Run benchmarks
make perf         # Quick performance test
```

### Building

```bash
# Debug build
make build

# Release build
make build-release

# Cross-platform builds
make cross-build

# Create distribution packages
make dist
```

### Testing

```bash
# Run all tests
make test

# Specific test types
make test-unit           # Unit tests only
make test-integration    # Integration tests only
make test-doc            # Documentation tests

# Test with coverage
make test-coverage

# Generate test fixtures
make fixtures
```

### Available Make Targets

#### Development
- `make dev` - Start development mode with file watching
- `make run` - Run in debug mode
- `make example` - Run example with sample input

#### Building
- `make build` - Debug build
- `make build-release` - Release build
- `make install` - Install binary
- `make cross-build` - Build for all platforms

#### Testing
- `make test` - Run all tests
- `make test-unit` - Run unit tests only
- `make test-integration` - Run integration tests only
- `make test-coverage` - Generate coverage report
- `make bench` - Run benchmarks

#### Code Quality
- `make check` - Run cargo check
- `make clippy` - Run clippy linter
- `make fmt` - Format code
- `make audit` - Security audit
- `make qa` - All quality checks

#### Documentation
- `make doc` - Generate docs
- `make doc-open` - Generate and open docs

#### Distribution
- `make dist` - Create release packages
- `make docker-build` - Build Docker image
- `make release` - Full release process

#### Maintenance
- `make clean` - Clean build artifacts
- `make update` - Update dependencies
- `make info` - Show project information

## Architecture

```
tfdiff/
├── src/
│   ├── main.rs              # CLI entry point
│   ├── lib.rs               # Library root
│   ├── input.rs             # Local and S3 input handling
│   ├── parser/              # Terraform output parsing
│   │   ├── cleaner.rs       # Input cleaning
│   │   ├── terraform.rs     # Plan/apply parsing and report comparison
│   │   └── diff.rs          # Resource diff extraction
│   ├── formatter/           # Output formatting
│   │   ├── terminal.rs      # Terminal output
│   │   ├── json.rs          # JSON export
│   │   ├── html.rs          # HTML reports
│   │   └── markdown.rs      # Markdown export
│   ├── ui/                  # User interfaces
│   │   ├── terminal.rs      # Terminal UI
│   │   └── web.rs           # Web server (planned)
│   └── models/              # Data structures
│       └── mod.rs           # Core models
├── tests/                   # Test suite
└── benches/                 # Benchmarks
```

## Contributing

1. Fork the repository
2. Create a feature branch: `git checkout -b my-feature`
3. Make your changes and ensure tests pass: `make qa`
4. Commit your changes: `git commit -am 'Add feature'`
5. Push to the branch: `git push origin my-feature`
6. Submit a pull request

### Code Guidelines

- Follow Rust best practices and idioms
- Add tests for new functionality
- Update documentation as needed
- Run `make qa` before submitting PR
- Keep commits focused and well-described

## ⚡ Performance

Tfdiff is optimized for speed and efficiency:

- **Parse Speed**: 10MB+ Terraform outputs parsed in <100ms
- **Memory Efficient**: Streaming parser minimizes memory usage
- **Regex Optimization**: Compiled regexes cached with lazy_static for 2x speedup
- **Simple HTML reports**: Responsive output with side-by-side value comparison

### Benchmarks
```bash
# Run performance benchmarks
make bench

# Quick performance test with large fixture
make perf

# Test with your own large fixture
time terraform plan | tfdiff --summary
```

## 🧪 Testing Strategy

The project includes unit, integration, property, performance, and output-format tests.

### Test Types
- **Unit Tests**: Individual function and module testing
- **Integration Tests**: End-to-end CLI workflows and file processing
- **Property Tests**: Fuzz testing with random inputs using proptest
- **Performance Tests**: Benchmarking critical parsing paths
- **Fixture Tests**: Real Terraform output samples from various providers

## 📖 CLI Reference

### Command Syntax
```bash
tfdiff [INPUT] [OPTIONS]
tfdiff OLDER LATER [OPTIONS]
```

### Arguments
- `INPUT` - Input file path (optional, defaults to stdin); one S3 object input compares its selected version with its predecessor
- `OLDER LATER` - Two snapshots/objects to compare in the supplied order

### Options

#### Output Format
- `--format <FORMAT>` - Output format: `terminal` (default), `json`, `html`, `markdown`
- `--browser` - Open HTML output in the default browser
- `--summary` - Show only summary information
- `--quiet` - Minimal output mode
- `--verbose` - Verbose output with extra details

#### Filtering and resource identity
- `-F, --filter <ACTIONS>` - Comma-separated list of actions to show: `create`, `update`, `destroy`, `read`, `noop`
- `--skip-data` - Omit data-resource changes; conflicts with `--only-data`
- `--only-data` - Show only data-resource changes; conflicts with `--skip-data`
- `--skip-ids` - Suppress the additional `id`, `arn`, or fallback `name` label for changed resources
- `--show-authors` - Summarize observed CloudTrail resource actors for eligible S3-backed state comparisons
- `--show-atomic-authors` - Show observed CloudTrail actors beside each changed resource
- `--show-full-attribution` - Show full audit attribution detail (principal ARNs, event IDs, sessions, clients) under each resource and in the report header; without it, a resolved principal email is shown after the resource header only (for example `@ext.user@example.com`), and no display name is captured
- `--author-region <REGION>` - Add a CloudTrail lookup region (repeatable; otherwise use the configured AWS region)
- `--aws-iam-region <REGION>` - Global-service lookup region for `aws_iam_*` resources (default `us-east-1`; configured regions are the fallback)
- `--aws-global-region <REGION>` - Global-service lookup region for other global resource types (default `us-east-1`; configured regions are the fallback)
- `--author-log-group <ARN>` - Configure an explicit CloudWatch Logs group for future historical-event supplementation (repeatable)
- `--author-window <DURATION>` - Look back from the later S3 `LastModified` timestamp (positive duration up to `24h`; default `5m`)
- Changed resources display one identity using `id`, then `arn`, then `name` priority when a non-sensitive value is available. Markdown places identity values in correctly delimited code spans; HTML escapes them as text. These are presentation controls, not redaction: JSON retains the full records and attributes, and reports can contain confidential values.

#### Output control
- `-p, --pager` - Use the configured pager only when stdout is a terminal; browser mode and `--unbuffer` bypass it
- `--unbuffer` - Write the fully rendered output one line at a time and flush each line; it does not stream parsing/formatting
- `--raw-secrets` - Show secrets in plain text; by default Terraform-sensitive values, attributes named like `password`/`token`/`secret`/`private_key` and AWS access key IDs (`AKIA…`) are masked
- `--color <WHEN>` - Terminal color policy: `always`, `auto` (default), or `never`; non-terminal formats never include ANSI styling
- `--port <PORT>` - Web server port (default: 8080; long option only; `-p` now means `--pager`)

#### Web Mode (Planned)
- `--web` - Start web server mode
- `--port <PORT>` - Web server port (default: 8080)

#### File Processing
- `--watch` - Watch input file for changes (planned)
- `--output <FILE>` - Output file (default: stdout)

#### Debugging
- `--debug` - Enable debug logging
- `--trace` - Enable trace logging

### Examples

#### Basic Usage
```bash
# Read from file
tfdiff terraform.plan

# Read from stdin
terraform plan | tfdiff

# Specify format
tfdiff plan.txt --format json
```

#### Browser Mode
```bash
# Open in browser
tfdiff plan.txt --browser
```

#### Filtering
```bash
# Show only creates and updates
tfdiff plan.txt --filter create,update

# Show only destroys
tfdiff plan.txt --filter destroy

# Combine with other options
tfdiff plan.txt --filter create --format json
```

#### Output Control
```bash
# Summary only
tfdiff plan.txt --summary

# Quiet mode
tfdiff plan.txt --quiet

# Verbose details
tfdiff plan.txt --verbose

# Save to file
tfdiff plan.txt --format html > report.html
```

#### Help and Version
```bash
# Show help
tfdiff --help
tfdiff -h

# Show version
tfdiff --version
tfdiff -V
```

### Exit Codes
- `0` - Success
- `1` - General error (invalid input, parsing failure)
- `2` - Invalid arguments or options
- `3` - File not found or permission error

### Environment Variables
- `TFDIFF_PAGER` - Pager command and arguments; takes precedence over `PAGER`, and is invoked directly without shell evaluation
- `PAGER` - Fallback pager command and arguments; defaults to `less -R` when neither pager variable is set
- `NO_COLOR` - Disable automatic terminal colors when set to any value; explicit `--color always` overrides it
- `TFDIFF_NO_COLOR` - Disable automatic terminal colors when set to `1`, `true`, or `yes`; explicit `--color always` overrides it
- `CLICOLOR_FORCE` - Force colors in `auto` mode when nonempty and not `0`, unless disabled by `NO_COLOR` or `TFDIFF_NO_COLOR`
- `TFDIFF_NO_CACHE` - Disable the local S3 cache
- `TFDIFF_BROWSER` - Override default browser command
- `RUST_LOG` - Control logging level (debug, trace)

## 🎨 Output Examples

### Terminal Output
```
🌊 TERRAFORM PLAN DIFF

📊 SUMMARY
✚ 2 resources to add
↻ 1 resource to change
✖ 0 resources to destroy

🔧 RESOURCES

✚ CREATE aws_s3_bucket.main
  └─ bucket = "my-terraform-bucket"
  └─ tags = {
    ├─ Name = "MainBucket"
    └─ Environment = "production"

↻ UPDATE aws_instance.web
  └─ instance_type = "t2.micro" → "t3.small"
```

## License

Apache License 2.0 - see [LICENSE](LICENSE) file for details.

## 🆘 Support & Contributing

### Getting Help
- 🐛 **Bug Reports**: [GitHub Issues](https://github.com/yourusername/tfdiff/issues)
- 💡 **Feature Requests**: [GitHub Discussions](https://github.com/yourusername/tfdiff/discussions)
- 📖 **Documentation**: [docs/](docs/) and this README
- 💬 **Questions**: Use GitHub Discussions for usage questions

### Contributing
We welcome contributions! See our [Contributing Guide](CONTRIBUTING.md):

1. 🍴 Fork the repository
2. 🌟 Create a feature branch: `git checkout -b amazing-feature`
3. ✅ Ensure tests pass: `make qa`
4. 📝 Add tests for new functionality
5. 🚀 Submit a pull request

See [CHANGELOG.md](CHANGELOG.md) for detailed release history.
