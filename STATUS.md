# Project Status

Updated: 2026-10-14

## Active work

Task #46: **Integrate audit attribution into CLI**. The approved implementation plan is `/home/markusg/.claude/plans/peppy-noodling-cray.md`.

The work is in progress. S3 source-version metadata preservation and the injectable CloudTrail/STS transport/query groundwork are complete (tasks #44 and #45). Aggregate attribution models and terminal/Markdown/HTML rendering groundwork also exist. The CLI does not yet enrich comparisons with audit evidence end to end.

## Remaining implementation

- Validate eligible author-enabled inputs and print the exact 90-day CloudTrail retention notice to stderr exactly once, including invalid-input and quiet paths.
- Enrich only filtered managed-resource changes for supported S3-backed comparisons, using the right-side S3 `LastModified` as the exclusive audit-window anchor. Do not use local-file mtime or invocation time.
- Implement sensitivity-aware resource identity extraction and exact, service-specific CloudTrail matching. The only currently allowlisted adapter is the VPC endpoint adapter; do not broaden support without validated adapters and synthetic fixtures.
- Wire attribution through ordinary comparisons, inventory/batch, summary, browser, and JSON outputs. Add per-resource atomic evidence and render it when `--show-atomic-authors` is enabled.
- Add bounded asynchronous lookup, progress reporting, audit-evidence caching, and explicitly configured CloudWatch Logs supplementation as described in the plan.
- Update README support, permissions, limitations, and correlation/non-causality documentation; add offline CLI, matching, formatter, filtering, and local-mock tests.
- Run focused tests, then `rtk proxy cargo test --all-targets`, `rtk proxy cargo fmt --check`, and `rtk proxy cargo clippy --all-targets --all-features -- -D warnings`.

## Current integration gaps

- `src/main.rs` currently has no CloudTrail enrichment call site and rejects some author-enabled input shapes that the plan intends to support.
- Clap argument parsing happens before `run()`, so the notice may be skipped on parse errors unless notice handling is moved to an appropriate earlier path.
- State sensitivity information is not currently exposed in a form suitable for safe downstream identity matching.
- Resource-level attribution formatting and actor retention in summary-only inventory/report output remain to be implemented.

## Workspace safeguards

- Preserve all existing uncommitted work; do not commit or push.
- Do not modify `older.tfstate`, `later.tfstate`, or `comparison.json`.
- Automated verification must use synthetic inputs and local AWS mocks only. The one-off, narrowly scoped live AWS research is complete; do not make further live AWS calls.
- The research sample had a right-side S3 version timestamp of `2026-05-08T12:04:53Z`, outside CloudTrail Event History's 90-day retention as of the research. No historical actor was verified; never attribute it to the current STS caller.

## Working tree at session start

The repository already had numerous modified and untracked files before this status note was created, including changes across the CLI, parsers, formatters, tests, and S3 support. Untracked user data included `older.tfstate`, `later.tfstate`, and `comparison.json`; preserve them unchanged. New audit groundwork includes `src/authors.rs`, and a new S3 test file is `tests/integration/s3_input_test.rs`.

No source implementation or tests were changed/run in the latest continuation before this status file was written.
