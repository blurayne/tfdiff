use clap::{CommandFactory, Parser};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tfdiff::formatter::format_json_output;
use tfdiff::{compare_states, parse_terraform_output, Result, TfdiffError};

#[derive(Parser)]
#[command(name = "tfdiff")]
#[command(about = "Compare Terraform plans and state snapshots, including S3 history")]
#[command(version)]
struct Cli {
    /// Input file (reads from stdin if not provided); add a second path to compare states
    #[arg(value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Output format
    #[arg(short, long, value_enum, default_value = "terminal", global = true)]
    format: OutputFormat,

    /// Shortcut for --format json
    #[arg(long, global = true, conflicts_with_all = ["md", "html"])]
    json: bool,

    /// Shortcut for --format markdown
    #[arg(long, global = true, conflicts_with_all = ["json", "html"])]
    md: bool,

    /// Shortcut for --format html
    #[arg(long, global = true, conflicts_with_all = ["json", "md"])]
    html: bool,

    /// Filter by action type: create, update, destroy, read, noop (comma-separated)
    #[arg(
        short = 'F',
        long = "filter-actions",
        visible_alias = "filter-action",
        alias = "filter",
        value_delimiter = ',',
        global = true
    )]
    filter: Vec<String>,

    /// Keep only resources whose address, type, or name matches PATTERN (repeatable)
    #[arg(long, value_name = "PATTERN", global = true)]
    filter_resource_name: Vec<String>,

    /// Keep only resources whose ARN matches PATTERN (repeatable)
    #[arg(long, value_name = "PATTERN", global = true)]
    filter_resource_arn: Vec<String>,

    /// Keep only resources whose ID matches PATTERN (repeatable)
    #[arg(long, value_name = "PATTERN", global = true)]
    filter_resource_id: Vec<String>,

    /// Force resource filters and S3 key patterns to be globs (default: auto-detected)
    #[arg(long, global = true, conflicts_with = "regex")]
    glob: bool,

    /// Force resource filters and S3 key patterns to be anchored regular expressions (default: auto-detected)
    #[arg(long, global = true, conflicts_with = "glob")]
    regex: bool,

    /// Omit data.* resource changes from the diff
    #[arg(long, global = true, conflicts_with = "only_data")]
    skip_data: bool,

    /// Show only data.* resource changes in the diff
    #[arg(long, global = true, conflicts_with = "skip_data")]
    only_data: bool,

    /// Omit resource IDs, ARNs, or fallback names from changed resources
    #[arg(long, global = true)]
    skip_ids: bool,

    /// Sort by action (create, update, destroy, read, noop), then resource type, name, and ID
    #[arg(long, value_enum, global = true)]
    sort_by: Option<SortBy>,

    /// Launch web UI
    #[arg(short, long, global = true)]
    web: bool,

    /// Open formatted output in the default pager
    #[arg(short = 'p', long, global = true)]
    pager: bool,

    /// Write rendered output one line at a time, flushing after each line
    #[arg(long, global = true)]
    unbuffer: bool,

    /// Use the full decorative terminal layout (header banner, S3 diff decorations)
    #[arg(long, global = true)]
    pretty: bool,

    /// Show full timestamps for S3 resource dates instead of relative ages
    #[arg(long, global = true)]
    show_datetime: bool,

    /// Terminal color policy
    #[arg(long, value_enum, default_value = "auto", global = true)]
    color: ColorWhen,

    /// AWS region override for S3 and audit requests
    #[arg(long, value_name = "REGION", global = true)]
    aws_region: Option<String>,

    /// Port for web UI
    #[arg(long, default_value = "8080", global = true)]
    port: u16,

    /// Watch file for changes
    #[arg(short = 'W', long, global = true)]
    watch: bool,

    /// Verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Quiet mode (minimal output; with --compact also hides source files)
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Show summary only
    #[arg(short, long, global = true)]
    summary: bool,

    /// Generate HTML and open in browser
    #[arg(short, long, global = true)]
    browser: bool,

    /// Show secrets in plain text. By default Terraform-sensitive values, attributes named like password/token/secret/private_key and AWS access key IDs (AKIA...) are masked
    #[arg(long, global = true)]
    raw_secrets: bool,

    /// Write redacted plan-shaped JSON comparison to a new file
    #[arg(long, value_name = "PATH", global = true)]
    generate_tfplan_file: Option<PathBuf>,

    /// Select the left S3 object version at or before this time (exact object pair, S3 history window, or --compact start; with --compact the default is the beginning of history)
    #[arg(long, global = true)]
    since: Option<String>,

    /// Select the right S3 object version at or before this time (exact object pair, S3 history window, or --compact end; default: now)
    #[arg(long, global = true)]
    until: Option<String>,

    /// Show one merged net-change diff between --since (default: beginning of history) and --until (default: now); requires an S3 prefix, glob, or exact object in a bucket with versioning Enabled
    #[arg(long, global = true)]
    compact: bool,

    /// With --compact, show the source state file and its from/until versions beside each created, updated, or destroyed resource (narrow with --filter-resource-*)
    #[arg(long, global = true)]
    show_atomic_sources: bool,

    /// With an S3 prefix or glob, show only resources that moved between state files (destroyed in one file, created in another with the same type and id/arn/name), sorted by resource
    #[arg(long, global = true)]
    only_state_migrations: bool,

    /// Show the branch history of S3 state files (moves, splits, merges) and mark OBSOLETE ones (read-only)
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["compact", "only_state_migrations", "show_atomic_sources", "generate_tfplan_file", "show_authors", "show_atomic_authors"]
    )]
    show_tfstate_duplicates: bool,

    /// With --show-tfstate-duplicates, print the full resource hash instead of its first 16 characters
    #[arg(long, global = true, requires = "show_tfstate_duplicates")]
    full_hash: bool,

    /// Default IANA timezone for date/time filters
    #[arg(long, value_name = "ZONE", global = true)]
    timezone: Option<String>,

    /// Interpret date/time filters in UTC
    #[arg(long, global = true, conflicts_with = "timezone")]
    utc: bool,

    /// Sort S3 inventory events by time
    #[arg(long, value_enum, global = true)]
    sort_order: Option<InventorySortOrder>,

    /// Omit delete-marker events from batch output
    #[arg(long, global = true)]
    skip_deleted: bool,

    /// Include all matching keys in batch output
    #[arg(long, global = true)]
    full_inventory: bool,

    /// Disable the local S3 object cache (also controlled by TFDIFF_NO_CACHE)
    #[arg(long, global = true)]
    no_cache: bool,

    /// Show updates as "- before" / "+ after" lines (default)
    #[arg(long, global = true, conflicts_with = "dual_pane")]
    single_pane: bool,

    /// Show updates with before and after values side by side
    #[arg(long, global = true, conflicts_with = "single_pane")]
    dual_pane: bool,

    /// Delete all entries from the local S3 object cache before running
    #[arg(long, global = true)]
    clear_cache: bool,

    /// Show observed resource actors for each eligible S3 state comparison
    #[arg(long, global = true)]
    show_authors: bool,

    /// Show observed actors beside each individual resource change
    #[arg(long, global = true)]
    show_atomic_authors: bool,

    /// Show full audit attribution detail (principals, event IDs, sessions, clients)
    #[arg(long, global = true)]
    show_full_attribution: bool,

    /// Add a CloudTrail lookup region (repeatable; default is the configured AWS region)
    #[arg(long = "author-region", value_name = "REGION", global = true)]
    author_regions: Vec<String>,

    /// Add an explicitly configured CloudWatch Logs group ARN (repeatable)
    #[arg(long = "author-log-group", value_name = "ARN", global = true)]
    author_log_groups: Vec<String>,

    /// CloudTrail look-back from the later S3 LastModified timestamp (default: 5m, max: 24h)
    #[arg(long = "author-window", value_name = "DURATION", global = true)]
    author_window: Option<String>,

    /// CloudTrail events requested per API page (1-50; default: 50)
    #[arg(long = "author-page-size", value_name = "N", global = true)]
    author_page_size: Option<u32>,

    /// Region for IAM CloudTrail lookups (default: us-east-1, then configured regions)
    #[arg(long = "aws-iam-region", value_name = "REGION", global = true)]
    aws_iam_region: Option<String>,

    /// Region for other global-service CloudTrail lookups (default: us-east-1, then configured regions)
    #[arg(long = "aws-global-region", value_name = "REGION", global = true)]
    aws_global_region: Option<String>,
}

impl Cli {
    /// Applies the --json/--md/--html shortcuts to `format`.
    fn resolve_format_shortcuts(&mut self) {
        if self.json {
            self.format = OutputFormat::Json;
        } else if self.md {
            self.format = OutputFormat::Markdown;
        } else if self.html {
            self.format = OutputFormat::Html;
        }
    }

    /// Mode forced by `--glob` / `--regex`, if either was given.
    fn forced_pattern_mode(&self) -> Option<tfdiff::pattern::PatternMode> {
        if self.regex {
            Some(tfdiff::pattern::PatternMode::Regex)
        } else if self.glob {
            Some(tfdiff::pattern::PatternMode::Glob)
        } else {
            None
        }
    }

    /// Mode for one value: an explicit flag wins, otherwise the value is auto-detected.
    fn pattern_mode_for(&self, value: &str) -> tfdiff::pattern::PatternMode {
        self.forced_pattern_mode()
            .or_else(|| tfdiff::pattern::detect_pattern_mode(value))
            .unwrap_or(tfdiff::pattern::PatternMode::Glob)
    }

    /// Mode for a set of values: regex only when the flag forces it or any value is detected as regex.
    fn pattern_mode_for_all<'a>(
        &self,
        values: impl IntoIterator<Item = &'a str>,
    ) -> tfdiff::pattern::PatternMode {
        if let Some(mode) = self.forced_pattern_mode() {
            return mode;
        }
        let detected: Vec<_> = values
            .into_iter()
            .filter_map(tfdiff::pattern::detect_pattern_mode)
            .collect();
        if detected.contains(&tfdiff::pattern::PatternMode::Regex) {
            tfdiff::pattern::PatternMode::Regex
        } else {
            tfdiff::pattern::PatternMode::Glob
        }
    }

    fn resource_filter(&self) -> Result<tfdiff::pattern::ResourceFilter> {
        let all_patterns = self
            .filter_resource_name
            .iter()
            .chain(&self.filter_resource_arn)
            .chain(&self.filter_resource_id)
            .map(String::as_str);
        tfdiff::pattern::ResourceFilter::new(
            &self.filter_resource_name,
            &self.filter_resource_arn,
            &self.filter_resource_id,
            self.pattern_mode_for_all(all_patterns),
        )
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum OutputFormat {
    Terminal,
    Json,
    Html,
    Markdown,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum InventorySortOrder {
    Asc,
    Desc,
}

impl From<InventorySortOrder> for tfdiff::inventory::SortOrder {
    fn from(value: InventorySortOrder) -> Self {
        match value {
            InventorySortOrder::Asc => Self::Asc,
            InventorySortOrder::Desc => Self::Desc,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum ColorWhen {
    Always,
    Auto,
    Never,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum SortBy {
    Action,
}

fn is_data_address(address: &str) -> bool {
    address
        .split('.')
        .any(|segment| segment == "data" || segment.starts_with("data["))
}

fn is_data_resource(resource: &tfdiff::Resource) -> bool {
    is_data_address(&resource.id) || resource.type_name.starts_with("data.")
}

fn filter_data_resources(plan: &mut tfdiff::TerraformPlan, skip_data: bool, only_data: bool) {
    if skip_data {
        plan.resources
            .retain(|resource| !is_data_resource(resource));
        plan.data_sources.retain(|source| {
            !is_data_address(&source.id) && !source.type_name.starts_with("data.")
        });
    } else if only_data {
        plan.resources.retain(is_data_resource);
        plan.data_sources
            .retain(|source| is_data_address(&source.id) || source.type_name.starts_with("data."));
    }
}

fn recalculate_summary(plan: &mut tfdiff::TerraformPlan) {
    plan.summary.add = 0;
    plan.summary.change = 0;
    plan.summary.destroy = 0;
    plan.summary.read = 0;
    for resource in &plan.resources {
        match (&plan.mode, &resource.plan_delta) {
            (tfdiff::PlanMode::PlanDiff, Some(tfdiff::PlanDelta::Added)) => {
                plan.summary.add += 1;
            }
            (tfdiff::PlanMode::PlanDiff, Some(tfdiff::PlanDelta::Changed)) => {
                plan.summary.change += 1;
            }
            (tfdiff::PlanMode::PlanDiff, Some(tfdiff::PlanDelta::Removed)) => {
                plan.summary.destroy += 1;
            }
            (_, _) => match resource.action {
                tfdiff::ActionType::Create => plan.summary.add += 1,
                tfdiff::ActionType::Update => plan.summary.change += 1,
                tfdiff::ActionType::Destroy => plan.summary.destroy += 1,
                tfdiff::ActionType::Read => plan.summary.read += 1,
                tfdiff::ActionType::NoOp => {}
            },
        }
    }
    plan.summary.read += plan.data_sources.len();
}

fn filter_plan(
    plan: &mut tfdiff::TerraformPlan,
    skip_data: bool,
    only_data: bool,
    actions: &[String],
    resources: &tfdiff::pattern::ResourceFilter,
) {
    filter_data_resources(plan, skip_data, only_data);
    if !resources.is_empty() {
        plan.resources
            .retain(|resource| resources.matches_resource(resource));
        plan.data_sources
            .retain(|source| resources.matches_data_source(source));
    }
    if !actions.is_empty() {
        plan.resources.retain(|resource| {
            let action = match resource.action {
                tfdiff::ActionType::Create => "create",
                tfdiff::ActionType::Update => "update",
                tfdiff::ActionType::Destroy => "destroy",
                tfdiff::ActionType::Read => "read",
                tfdiff::ActionType::NoOp => "noop",
            };
            actions.iter().any(|filter| filter == action)
        });
    }
    recalculate_summary(plan);
}

fn sort_resources(plan: &mut tfdiff::TerraformPlan, sort_by: Option<SortBy>) {
    if let Some(SortBy::Action) = sort_by {
        plan.resources.sort_by(|left, right| {
            action_sort_priority(&left.action)
                .cmp(&action_sort_priority(&right.action))
                .then_with(|| left.type_name.cmp(&right.type_name))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

fn write_output(
    output: &str,
    use_pager: bool,
    browser_mode: bool,
    unbuffer: bool,
    stdout_is_terminal: bool,
) -> Result<()> {
    let stdout = io::stdout();
    let stderr = io::stderr();
    write_output_with(
        output,
        OutputRouting {
            use_pager,
            browser_mode,
            unbuffer,
            stdout_is_terminal,
            pager: pager_command(),
        },
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
}

struct OutputRouting {
    use_pager: bool,
    browser_mode: bool,
    unbuffer: bool,
    stdout_is_terminal: bool,
    pager: Option<(String, Vec<String>)>,
}

fn write_output_with(
    output: &str,
    routing: OutputRouting,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    if routing.use_pager && !routing.browser_mode && !routing.unbuffer && routing.stdout_is_terminal
    {
        if let Some((program, arguments)) = routing.pager {
            match Command::new(&program)
                .args(arguments)
                .stdin(Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    let write_result = child
                        .stdin
                        .take()
                        .expect("pager stdin is piped")
                        .write_all(with_trailing_newline(output).as_bytes());
                    let status = child.wait();
                    if let Err(error) = write_result {
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            return Err(TfdiffError::IoError(error));
                        }
                    }
                    match status {
                        Ok(status) if status.success() => return Ok(()),
                        Ok(status) => {
                            return Err(TfdiffError::ParseError(format!(
                                "pager {program} exited with status {status}"
                            )))
                        }
                        Err(error) => return Err(TfdiffError::IoError(error)),
                    }
                }
                Err(error) => {
                    writeln!(
                        stderr,
                        "tfdiff: could not start pager {program}: {error}; writing to stdout"
                    )
                    .map_err(TfdiffError::IoError)?;
                }
            }
        }
    }

    write_stdout(stdout, output, routing.unbuffer)?;
    Ok(())
}

fn with_trailing_newline(output: &str) -> std::borrow::Cow<'_, str> {
    if output.ends_with('\n') {
        std::borrow::Cow::Borrowed(output)
    } else {
        std::borrow::Cow::Owned(format!("{output}\n"))
    }
}

fn write_stdout(writer: &mut impl Write, output: &str, unbuffer: bool) -> io::Result<()> {
    let output = with_trailing_newline(output);
    if unbuffer {
        for line in output.split_inclusive('\n') {
            writer.write_all(line.as_bytes())?;
            writer.flush()?;
        }
    } else {
        writer.write_all(output.as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

fn pager_command() -> Option<(String, Vec<String>)> {
    let tfdiff_pager = std::env::var("TFDIFF_PAGER").ok();
    let pager = std::env::var("PAGER").ok();
    pager_command_from_config(tfdiff_pager.as_deref(), pager.as_deref())
}

fn pager_command_from_config(
    tfdiff_pager: Option<&str>,
    pager: Option<&str>,
) -> Option<(String, Vec<String>)> {
    let configured = tfdiff_pager
        .filter(|value| !value.trim().is_empty())
        .or_else(|| pager.filter(|value| !value.trim().is_empty()));
    let words = configured
        .map(parse_command_words)
        .unwrap_or_else(|| vec!["less".to_string(), "-R".to_string()]);
    let mut words = words.into_iter();
    let program = words.next()?;
    Some((program, words.collect()))
}

fn parse_command_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                word.push(character);
            }
        } else if character.is_whitespace() && quote.is_none() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if escaped {
        word.push('\\');
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn resolve_color(mode: ColorWhen, stdout_is_terminal: bool) -> bool {
    let force_color =
        std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| !value.is_empty() && value != "0");
    resolve_color_with_env(
        mode,
        stdout_is_terminal,
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("TFDIFF_NO_COLOR").ok().as_deref(),
        force_color,
    )
}

fn resolve_color_with_env(
    mode: ColorWhen,
    stdout_is_terminal: bool,
    no_color: bool,
    tfdiff_no_color: Option<&str>,
    force_color: bool,
) -> bool {
    match mode {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            let tfdiff_no_color = tfdiff_no_color.is_some_and(|value| {
                matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            });
            if no_color || tfdiff_no_color {
                false
            } else {
                force_color || stdout_is_terminal
            }
        }
    }
}

fn action_sort_priority(action: &tfdiff::ActionType) -> u8 {
    match action {
        tfdiff::ActionType::Create => 0,
        tfdiff::ActionType::Update => 1,
        tfdiff::ActionType::Destroy => 2,
        tfdiff::ActionType::Read => 3,
        tfdiff::ActionType::NoOp => 4,
    }
}

fn open_in_browser(plan: &tfdiff::TerraformPlan, show_resource_identity: bool) -> Result<()> {
    let html_content = tfdiff::formatter::format_html_output_with_options(
        plan,
        tfdiff::formatter::FormatterOptions {
            show_resource_identity,
            show_full_attribution: false,
            color: false,
        },
    );
    let temp_dir = std::env::temp_dir();
    let temp_file_path = temp_dir.join(format!(
        "tfdiff_{}_{}.html",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut temp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_file_path)
        .map_err(TfdiffError::IoError)?;
    temp_file.write_all(html_content.as_bytes())?;

    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg(&temp_file_path).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", "start", temp_file_path.to_str().unwrap()])
            .status()
    } else {
        Command::new("xdg-open").arg(&temp_file_path).status()
    };
    match result {
        Ok(status) if status.success() => {
            println!(
                "🌐 Opened Terraform diff in browser: {}",
                temp_file_path.display()
            );
            Ok(())
        }
        Ok(_) => {
            eprintln!(
                "❌ Failed to open browser\n📄 HTML file saved to: {}",
                temp_file_path.display()
            );
            Ok(())
        }
        Err(error) => {
            eprintln!(
                "❌ Error opening browser: {error}\n📄 HTML file saved to: {}",
                temp_file_path.display()
            );
            Ok(())
        }
    }
}

fn author_flag_requested(args: &[std::ffi::OsString]) -> bool {
    let value_options = [
        "--format",
        "--filter",
        "--aws-region",
        "--sort-by",
        "--port",
        "--color",
        "--pretty",
        "--generate-tfplan-file",
        "--since",
        "--until",
        "--timezone",
        "--sort-order",
        "--author-region",
        "--author-log-group",
        "--author-window",
        "--author-page-size",
        "--aws-iam-region",
        "--aws-global-region",
        "-F",
        "-f",
    ];
    let mut index = 1;
    while let Some(argument) = args.get(index).and_then(|argument| argument.to_str()) {
        if argument == "--" {
            break;
        }
        if argument == "--show-authors"
            || argument.starts_with("--show-authors=")
            || argument == "--show-atomic-authors"
            || argument.starts_with("--show-atomic-authors=")
        {
            return true;
        }
        let option = argument.split_once('=').map_or(argument, |(name, _)| name);
        if argument == option && value_options.contains(&option) {
            index += 1;
        }
        index += 1;
    }
    false
}

fn main() -> std::process::ExitCode {
    match run_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if author_flag_requested(&args) {
        eprintln!("{}", tfdiff::authors::RETENTION_NOTICE);
    }
    let mut cli = Cli::parse_from(args);
    cli.resolve_format_shortcuts();
    if cli.inputs.is_empty() && io::stdin().is_terminal() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(TfdiffError::IoError)?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let author_options = tfdiff::authors::AuthorOptions {
        show_authors: cli.show_authors,
        show_atomic_authors: cli.show_atomic_authors,
        regions: cli.author_regions.clone(),
        log_groups: cli.author_log_groups.clone(),
        window: cli
            .author_window
            .as_deref()
            .map(tfdiff::authors::parse_author_window)
            .transpose()?
            .unwrap_or(tfdiff::authors::DEFAULT_AUTHOR_WINDOW),
        page_size: cli
            .author_page_size
            .unwrap_or(tfdiff::authors::DEFAULT_CLOUDTRAIL_PAGE_SIZE),
        iam_region: cli.aws_iam_region.clone(),
        global_region: cli.aws_global_region.clone(),
        auxiliary_options_provided: !cli.author_regions.is_empty()
            || !cli.author_log_groups.is_empty()
            || cli.author_window.is_some()
            || cli.author_page_size.is_some()
            || cli.aws_iam_region.is_some()
            || cli.aws_global_region.is_some(),
    };
    author_options.validate()?;
    let now = chrono::Utc::now();
    tfdiff::formatter::terminal::set_dual_pane(cli.dual_pane);
    tfdiff::secrets::set_raw_secrets(cli.raw_secrets);
    let cache_policy = tfdiff::input::S3CachePolicy::from_options(cli.no_cache)?;
    if cli.clear_cache {
        if let Some(cache) = cache_policy.cache() {
            cache.clear()?;
        }
    }
    let s3_config = tfdiff::input::S3ClientConfig::new(cli.aws_region.clone());
    if cli.inputs.len() > 2 {
        return Err(argument_error(
            "at most two positional input paths are accepted",
        ));
    }
    let paths = cli.inputs.clone();
    let state_diff = cli.inputs.len() == 2;
    let resource_filter = cli.resource_filter()?;
    if cli.show_atomic_sources && !cli.compact {
        return Err(argument_error("--show-atomic-sources requires --compact"));
    }
    if cli.compact {
        validate_compact_arguments(&cli, &paths)?;
    }
    const MIGRATIONS_SHAPE: &str =
        "--only-state-migrations requires an S3 prefix or glob input without --compact";
    if cli.only_state_migrations && (cli.compact || paths.len() != 1) {
        return Err(argument_error(MIGRATIONS_SHAPE));
    }
    if cli.show_tfstate_duplicates {
        return run_tfstate_duplicates(&cli, &paths, &cache_policy, &s3_config).await;
    }
    if cli.generate_tfplan_file.is_some() && !state_diff {
        return Err(argument_error(
            "--generate-tfplan-file requires two state snapshot paths",
        ));
    }
    let sources = paths
        .iter()
        .map(|path| {
            let mode = cli.pattern_mode_for(&path.to_string_lossy());
            tfdiff::input::InputSource::parse_with_mode(path, mode)
        })
        .collect::<Result<Vec<_>>>()?;
    let batch_options = tfdiff::inventory::InventoryOptions {
        since: cli.since.clone(),
        until: cli.until.clone(),
        timezone: cli.timezone.clone(),
        utc: cli.utc,
        sort_order: cli.sort_order.unwrap_or(InventorySortOrder::Desc).into(),
        skip_deleted: cli.skip_deleted,
        full_inventory: cli.full_inventory,
    };
    let uses_batch_options = cli.since.is_some()
        || cli.until.is_some()
        || cli.timezone.is_some()
        || cli.utc
        || cli.skip_deleted
        || cli.full_inventory
        || cli.sort_order.is_some();
    let has_time_bounds = cli.since.is_some() || cli.until.is_some();
    let inventory_source = match sources.as_slice() {
        [tfdiff::input::InputSource::S3Inventory(source)] => Some(source),
        _ => None,
    };
    if cli.only_state_migrations && inventory_source.is_none() {
        return Err(argument_error(MIGRATIONS_SHAPE));
    }
    if author_options.enabled() {
        validate_author_source_shape(&sources, inventory_source.is_some())?;
    }
    if cli.compact {
        let source = sources.first().expect("validated by --compact checks");
        tfdiff::inventory::validate_options(&batch_options)?;
        let mut report = tfdiff::compact::build_compact_report(
            source,
            &batch_options,
            &cache_policy,
            &s3_config,
            cli.unbuffer,
            resolve_color(cli.color, io::stderr().is_terminal()),
        )
        .await?;
        if let Some(warning) = &report.since_warning {
            let color = resolve_color(cli.color, io::stderr().is_terminal());
            if color {
                eprintln!("\x1b[33mWarning: {warning}\x1b[0m");
            } else {
                eprintln!("Warning: {warning}");
            }
        }
        filter_plan(
            &mut report.plan,
            cli.skip_data,
            cli.only_data,
            &cli.filter,
            &resource_filter,
        );
        sort_resources(&mut report.plan, cli.sort_by);
        tfdiff::compact::refresh_compact_sources(&mut report);
        tfdiff::compact::apply_source_display(
            &mut report,
            cli.quiet,
            cli.show_atomic_sources,
            matches!(cli.format, OutputFormat::Json) && !cli.browser,
        );
        let terminal_color = resolve_color(
            cli.color,
            io::stdout().is_terminal() || (cli.pager && !cli.unbuffer),
        );
        let plain_options = tfdiff::formatter::FormatterOptions {
            show_resource_identity: !cli.skip_ids,
            show_full_attribution: cli.show_full_attribution,
            color: false,
        };
        let output = if cli.summary && matches!(cli.format, OutputFormat::Terminal) {
            tfdiff::compact::format_compact_summary(&report)
        } else {
            match cli.format {
                OutputFormat::Terminal => tfdiff::compact::format_compact_terminal(
                    &report,
                    tfdiff::formatter::FormatterOptions {
                        color: terminal_color,
                        ..plain_options
                    },
                    cli.quiet,
                    cli.pretty,
                    cli.show_datetime,
                ),
                OutputFormat::Json => tfdiff::compact::format_compact_json(&report)?,
                OutputFormat::Html => {
                    tfdiff::compact::format_compact_html_with_options(&report, plain_options)
                }
                OutputFormat::Markdown => {
                    tfdiff::compact::format_compact_markdown_with_options(&report, plain_options)
                }
            }
        };
        if cli.browser {
            let html = tfdiff::compact::format_compact_html_with_options(&report, plain_options);
            open_html_in_browser(&html, "tfdiff_compact")?;
        } else {
            write_output(
                &output,
                cli.pager,
                cli.browser,
                cli.unbuffer,
                io::stdout().is_terminal(),
            )?;
        }
        if !report.errors.is_empty() || !report.complete {
            return Err(TfdiffError::ParseError(format!(
                "S3 compact diff completed with {} error(s); partial results were rendered",
                report.counts.errors
            )));
        }
        return Ok(());
    }
    if let Some(source) = inventory_source {
        tfdiff::inventory::validate_options(&batch_options)?;
        let mut report = tfdiff::inventory::build_batch_report_cached_with_options_and_s3_config(
            source,
            &batch_options,
            &cache_policy,
            &s3_config,
            cli.unbuffer,
            resolve_color(cli.color, io::stderr().is_terminal()),
        )
        .await?;
        // Moves are detected before --filter drops creates/destroys, but after the resource
        // name/arn/id filters so those narrow the migrations too.
        if !resource_filter.is_empty() {
            for plan in report
                .events
                .iter_mut()
                .filter_map(|event| event.plan.as_mut())
            {
                plan.resources
                    .retain(|resource| resource_filter.matches_resource(resource));
            }
        }
        for plan in report
            .events
            .iter_mut()
            .filter_map(|event| event.plan.as_mut())
        {
            filter_data_resources(plan, cli.skip_data, cli.only_data);
        }
        report.migrations = tfdiff::migrations::detect_migrations(&report);
        if cli.only_state_migrations {
            return render_state_migrations(&cli, &report);
        }
        for event in &mut report.events {
            if let Some(plan) = &mut event.plan {
                filter_plan(
                    plan,
                    cli.skip_data,
                    cli.only_data,
                    &cli.filter,
                    &resource_filter,
                );
                sort_resources(plan, cli.sort_by);
            }
        }
        if author_options.enabled() {
            enrich_batch_report_authors(&mut report, &author_options, &s3_config).await;
        }
        let terminal_color = resolve_color(
            cli.color,
            io::stdout().is_terminal() || (cli.pager && !cli.unbuffer),
        );
        let identity_and_color = tfdiff::formatter::FormatterOptions {
            show_resource_identity: !cli.skip_ids,
            show_full_attribution: cli.show_full_attribution,
            color: terminal_color,
        };
        let output = if cli.summary && matches!(cli.format, OutputFormat::Terminal) {
            let mut summary = format!(
                "S3 history: {} matching keys | {} in-range events | {} diffs | {} deletions | {} non-state events skipped | {} expected skips | {} errors{}",
                report.counts.matching_keys,
                report.counts.in_range_events,
                report.counts.diffs,
                report.counts.deletions,
                report.counts.skipped_non_state,
                report.counts.expected_skips,
                report.counts.errors,
                if report.complete { "" } else { " | INCOMPLETE INVENTORY" }
            );
            if author_options.show_authors {
                summary.push_str(&format!("\n{}", format_batch_author_summary(&report)));
            }
            summary
        } else {
            match cli.format {
                OutputFormat::Terminal => {
                    tfdiff::inventory::format_batch_terminal_with_display_options(
                        &report,
                        identity_and_color,
                        cli.quiet,
                        cli.pretty,
                        cli.show_datetime,
                    )
                }
                OutputFormat::Json => tfdiff::inventory::format_batch_json(&report)?,
                OutputFormat::Html => tfdiff::inventory::format_batch_html_with_options(
                    &report,
                    tfdiff::formatter::FormatterOptions {
                        show_resource_identity: !cli.skip_ids,
                        show_full_attribution: cli.show_full_attribution,
                        color: false,
                    },
                ),
                OutputFormat::Markdown => tfdiff::inventory::format_batch_markdown_with_options(
                    &report,
                    tfdiff::formatter::FormatterOptions {
                        show_resource_identity: !cli.skip_ids,
                        show_full_attribution: cli.show_full_attribution,
                        color: false,
                    },
                ),
            }
        };
        if cli.browser {
            let html = tfdiff::inventory::format_batch_html_with_options(
                &report,
                tfdiff::formatter::FormatterOptions {
                    show_resource_identity: !cli.skip_ids,
                    show_full_attribution: cli.show_full_attribution,
                    color: false,
                },
            );
            open_html_in_browser(&html, "tfdiff_inventory")?;
        } else {
            write_output(
                &output,
                cli.pager,
                cli.browser,
                cli.unbuffer,
                io::stdout().is_terminal(),
            )?;
        }
        if report.counts.errors > 0 || !report.complete {
            return Err(TfdiffError::ParseError(format!(
                "S3 inventory completed with {} error(s); partial results were rendered",
                report.counts.errors
            )));
        }
        return Ok(());
    }
    let bounded_object = if has_time_bounds && inventory_source.is_none() {
        if state_diff || paths.len() != 1 {
            return Err(argument_error(
                "--since and --until time-pair selection require one exact S3 object input",
            ));
        }
        let object = match sources.as_slice() {
            [tfdiff::input::InputSource::S3(object)] => object,
            _ => {
                return Err(argument_error(
                    "--since and --until time-pair selection require one exact S3 object input",
                ))
            }
        };
        let since = cli
            .since
            .as_deref()
            .map(|value| tfdiff::inventory::parse_time_bound(value, true, &batch_options, now))
            .transpose()?;
        let until = cli
            .until
            .as_deref()
            .map(|value| tfdiff::inventory::parse_time_bound(value, false, &batch_options, now))
            .transpose()?;
        if cli.skip_deleted || cli.full_inventory || cli.sort_order.is_some() {
            return Err(argument_error(
                "--skip-deleted, --full-inventory, and --sort-order apply only to S3 history inventory",
            ));
        }
        Some((object, since, until))
    } else {
        None
    };
    if uses_batch_options && bounded_object.is_none() && inventory_source.is_none() {
        return Err(argument_error(
            "--since, --until, --timezone, --utc, non-default --sort-order, --skip-deleted, and --full-inventory require one S3 prefix or glob input, or exact-object time-pair selection",
        ));
    }
    if cli
        .generate_tfplan_file
        .as_deref()
        .is_some_and(tfdiff::input::InputSource::is_s3_uri)
    {
        return Err(argument_error(
            "--generate-tfplan-file destination must be a local path",
        ));
    }

    let mut export_json = None;
    let (plan, author_right_version) = if state_diff {
        let snapshots =
            tfdiff::input::load_inputs_cached_with_config(&sources, &cache_policy, &s3_config)
                .await?;
        let older = &snapshots[0];
        let later = &snapshots[1];
        let mut comparison = compare_states(&older.contents, &later.contents)?;
        comparison.plan.source_versions = source_versions(
            "Input 1",
            "Input 2",
            older.version.clone(),
            later.version.clone(),
        );
        if let Some(destination) = &cli.generate_tfplan_file {
            write_export(
                destination,
                &paths,
                &tfdiff::format_state_diff_export(&comparison.export)?,
            )?;
            export_json = Some(destination.display().to_string());
        }
        (comparison.plan, later.version.clone())
    } else if let Some((object, since, until)) = bounded_object {
        let (older, later) = tfdiff::input::read_version_diff_at_bounds_cached_with_config(
            object,
            since,
            until,
            now,
            &cache_policy,
            &s3_config,
        )
        .await?;
        let mut plan = if tfdiff::is_terraform_v4_state(&older.contents)
            && tfdiff::is_terraform_v4_state(&later.contents)
        {
            compare_states(&older.contents, &later.contents)?.plan
        } else if !tfdiff::is_terraform_v4_state(&older.contents)
            && !tfdiff::is_terraform_v4_state(&later.contents)
        {
            tfdiff::compare_plan_outputs(&older.contents, &later.contents)?
        } else {
            return Err(TfdiffError::ParseError(
                "S3 history entries contain different Terraform formats; cannot compare them"
                    .to_string(),
            ));
        };
        plan.source_versions =
            source_versions("Since", "Until", older.version, later.version.clone());
        (plan, later.version)
    } else if let Some(source @ tfdiff::input::InputSource::S3(_)) = sources.first() {
        let (older, later) =
            tfdiff::input::read_version_diff_cached_with_config(source, &cache_policy, &s3_config)
                .await?;
        let mut plan = if tfdiff::is_terraform_v4_state(&older.contents)
            && tfdiff::is_terraform_v4_state(&later.contents)
        {
            compare_states(&older.contents, &later.contents)?.plan
        } else if !tfdiff::is_terraform_v4_state(&older.contents)
            && !tfdiff::is_terraform_v4_state(&later.contents)
        {
            tfdiff::compare_plan_outputs(&older.contents, &later.contents)?
        } else {
            return Err(TfdiffError::ParseError(
                "S3 history entries contain different Terraform formats; cannot compare them"
                    .to_string(),
            ));
        };
        plan.source_versions = source_versions(
            "Previous Version",
            "Selected Version",
            older.version,
            later.version.clone(),
        );
        (plan, later.version)
    } else {
        let input = if let Some(source) = sources.first() {
            tfdiff::input::load_inputs_cached_with_config(
                std::slice::from_ref(source),
                &cache_policy,
                &s3_config,
            )
            .await?
            .pop()
            .expect("one input source was provided")
        } else {
            let mut buffer = String::new();
            io::stdin()
                .read_to_string(&mut buffer)
                .map_err(TfdiffError::IoError)?;
            tfdiff::input::LoadedInput {
                contents: buffer,
                version: None,
            }
        };
        (parse_terraform_output(&input.contents)?, None)
    };

    let mut filtered_plan = plan.clone();
    filter_plan(
        &mut filtered_plan,
        cli.skip_data,
        cli.only_data,
        &cli.filter,
        &resource_filter,
    );
    sort_resources(&mut filtered_plan, cli.sort_by);

    if author_options.enabled() {
        if filtered_plan.mode != tfdiff::PlanMode::StateDiff {
            return Err(argument_error(
                "author attribution is available only for Terraform state comparisons with a right-side S3 version",
            ));
        }
        let right_source = sources.get(1).or_else(|| {
            (sources.len() == 1 && matches!(sources[0], tfdiff::input::InputSource::S3(_)))
                .then(|| &sources[0])
        });
        if !right_source.is_some_and(|source| matches!(source, tfdiff::input::InputSource::S3(_))) {
            return Err(argument_error(
                "author attribution requires the later/right input to be an S3 object so its LastModified timestamp can define the audit window",
            ));
        }
        let right_last_modified = author_right_version
            .as_ref()
            .and_then(|version| version.last_modified.as_deref())
            .or_else(|| {
                filtered_plan
                    .source_versions
                    .as_ref()
                    .and_then(|versions| versions.right.as_ref())
                    .and_then(|version| version.last_modified.as_deref())
            });
        if right_last_modified.is_none() {
            filtered_plan.author_attribution = Some(tfdiff::authors::AuditReport {
                window_start: String::new(),
                window_end: String::new(),
                regions: cli.author_regions.clone(),
                show_atomic_authors: author_options.show_atomic_authors,
                status: tfdiff::authors::AuditStatus::Incomplete,
                message: Some(
                    "right-side S3 LastModified is unavailable; no audit window was inferred"
                        .to_string(),
                ),
                observations: Vec::new(),
            });
            eprintln!("tfdiff: audit attribution incomplete: right-side S3 LastModified is unavailable; no audit requests were made");
        } else if tfdiff::authors::AuthorOptions::has_lookup_candidates(&filtered_plan) {
            let aws_config = s3_config.sdk_config().await;
            let default_region = aws_config
                .region()
                .map(|region| region.as_ref().to_string());
            let transport = tfdiff::authors::AwsAuditTransport::from_sdk_config(&aws_config);
            let report = tfdiff::authors::enrich_plan_with_transport(
                &mut filtered_plan,
                &author_options,
                Some(&transport),
                default_region.as_deref(),
                chrono::Utc::now(),
            )
            .await;
            if let Some(message) = report.message.as_deref() {
                eprintln!("tfdiff: audit attribution: {message}");
            }
        } else {
            let report = tfdiff::authors::enrich_plan_with_transport(
                &mut filtered_plan,
                &author_options,
                None,
                None,
                chrono::Utc::now(),
            )
            .await;
            if let Some(message) = report.message.as_deref() {
                eprintln!("tfdiff: audit attribution: {message}");
            }
        }
    }

    if cli.web {
        eprintln!("Web UI mode on port {} (not yet implemented)", cli.port);
        return Ok(());
    }

    let output = match cli.format {
        OutputFormat::Terminal => {
            let s3_uri = if !state_diff {
                sources.first().and_then(|source| match source {
                    tfdiff::input::InputSource::S3(object) => {
                        Some(format!("s3://{}/{}", object.bucket, object.key))
                    }
                    _ => None,
                })
            } else {
                None
            };
            tfdiff::formatter::format_terminal_output_with_s3_provenance_pretty(
                &filtered_plan,
                tfdiff::formatter::FormatterOptions {
                    show_resource_identity: !cli.skip_ids,
                    show_full_attribution: cli.show_full_attribution,
                    color: resolve_color(
                        cli.color,
                        io::stdout().is_terminal() || (cli.pager && !cli.unbuffer),
                    ),
                },
                s3_uri.as_deref(),
                cli.pretty,
                cli.show_datetime,
            )
        }
        OutputFormat::Json => format_json_output(&filtered_plan)?,
        OutputFormat::Html => tfdiff::formatter::format_html_output_with_options(
            &filtered_plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: !cli.skip_ids,
                show_full_attribution: cli.show_full_attribution,
                color: false,
            },
        ),
        OutputFormat::Markdown => tfdiff::formatter::format_markdown_output_with_options(
            &filtered_plan,
            tfdiff::formatter::FormatterOptions {
                show_resource_identity: !cli.skip_ids,
                show_full_attribution: cli.show_full_attribution,
                color: false,
            },
        ),
    };
    if matches!(cli.format, OutputFormat::Json) {
        if let Some(versions) = &filtered_plan.source_versions {
            eprintln!(
                "Source versions: {}={}{}; {}={}{}",
                versions.left_label,
                versions
                    .left
                    .as_ref()
                    .and_then(|version| version.version_id.as_deref())
                    .unwrap_or("unknown"),
                versions
                    .left
                    .as_ref()
                    .and_then(|version| version.last_modified.as_deref())
                    .map(|timestamp| format!(" ({timestamp})"))
                    .unwrap_or_default(),
                versions.right_label,
                versions
                    .right
                    .as_ref()
                    .and_then(|version| version.version_id.as_deref())
                    .unwrap_or("unknown"),
                versions
                    .right
                    .as_ref()
                    .and_then(|version| version.last_modified.as_deref())
                    .map(|timestamp| format!(" ({timestamp})"))
                    .unwrap_or_default(),
            );
        }
    }
    if cli.browser {
        return open_in_browser(&filtered_plan, !cli.skip_ids);
    }
    if cli.summary && matches!(cli.format, OutputFormat::Terminal) {
        let mut summary = if filtered_plan.mode == tfdiff::PlanMode::StateDiff {
            format!(
                "Observed differences: {} added, {} changed, {} removed",
                filtered_plan.summary.add,
                filtered_plan.summary.change,
                filtered_plan.summary.destroy
            )
        } else {
            format!(
                "Summary: {} to add, {} to change, {} to destroy",
                filtered_plan.summary.add,
                filtered_plan.summary.change,
                filtered_plan.summary.destroy
            )
        };
        if cli.show_authors {
            summary.push_str(&format!(
                "\n{}",
                format_author_summary(std::iter::once(&filtered_plan))
            ));
        }
        write_output(
            &summary,
            cli.pager,
            cli.browser,
            cli.unbuffer,
            io::stdout().is_terminal(),
        )?;
    } else {
        write_output(
            &output,
            cli.pager,
            cli.browser,
            cli.unbuffer,
            io::stdout().is_terminal(),
        )?;
    }
    if let Some(path) = export_json {
        eprintln!("Wrote redacted, partial Terraform-show-like comparison JSON to {path}; this is not a state file or applyable plan.");
    }
    Ok(())
}

fn validate_author_source_shape(
    sources: &[tfdiff::input::InputSource],
    inventory: bool,
) -> Result<()> {
    if inventory {
        return Ok(());
    }
    if sources.len() == 1 && matches!(sources.first(), Some(tfdiff::input::InputSource::S3(_))) {
        return Ok(());
    }
    if sources.len() == 2 {
        return match sources.get(1) {
            Some(tfdiff::input::InputSource::S3(_)) => Ok(()),
            _ => Err(argument_error(
                "--show-authors and --show-atomic-authors require the later/right input to be an S3 object so its LastModified timestamp can define the audit window",
            )),
        };
    }
    Err(argument_error(
        "--show-authors and --show-atomic-authors require an S3 object history diff, a two-input comparison with a right-side S3 object, or S3 inventory",
    ))
}

async fn enrich_batch_report_authors(
    report: &mut tfdiff::inventory::BatchReport,
    options: &tfdiff::authors::AuthorOptions,
    s3_config: &tfdiff::input::S3ClientConfig,
) {
    let has_candidates = report
        .events
        .iter()
        .filter_map(|event| event.plan.as_ref())
        .any(tfdiff::authors::AuthorOptions::has_lookup_candidates);
    let aws_config = if has_candidates {
        Some(s3_config.sdk_config().await)
    } else {
        None
    };
    let default_region = aws_config
        .as_ref()
        .and_then(|config| config.region())
        .map(|region| region.as_ref().to_string());
    let transport = aws_config
        .as_ref()
        .map(tfdiff::authors::AwsAuditTransport::from_sdk_config);
    let mut printed_messages = std::collections::HashSet::new();

    for event in &mut report.events {
        let Some(plan) = &mut event.plan else {
            continue;
        };
        let attribution = if !tfdiff::authors::AuthorOptions::has_lookup_candidates(plan) {
            tfdiff::authors::enrich_plan_with_transport(
                plan,
                options,
                None,
                default_region.as_deref(),
                chrono::Utc::now(),
            )
            .await
        } else {
            tfdiff::authors::enrich_plan_with_transport(
                plan,
                options,
                transport
                    .as_ref()
                    .map(|transport| transport as &dyn tfdiff::authors::AuditTransport),
                default_region.as_deref(),
                chrono::Utc::now(),
            )
            .await
        };
        if let Some(message) = attribution.message.as_deref() {
            if printed_messages.insert(message.to_string()) {
                eprintln!("tfdiff: audit attribution: {message}");
            }
        }
    }
}

fn format_batch_author_summary(report: &tfdiff::inventory::BatchReport) -> String {
    format_author_summary(report.events.iter().filter_map(|event| event.plan.as_ref()))
}

fn format_author_summary<'a>(plans: impl IntoIterator<Item = &'a tfdiff::TerraformPlan>) -> String {
    use std::collections::HashSet;

    let mut principals = HashSet::new();
    let mut event_ids = HashSet::new();
    let mut matched_resources = 0;
    let mut statuses = HashSet::new();
    for plan in plans {
        if let Some(attribution) = &plan.author_attribution {
            statuses.insert(format!("{:?}", attribution.status));
            for observation in &attribution.observations {
                if let Some(principal) = observation
                    .principal_arn
                    .as_deref()
                    .or(observation.principal_id.as_deref())
                {
                    principals.insert(principal.to_string());
                }
                event_ids.insert(observation.event_id.clone());
            }
        }
        matched_resources += plan
            .resources
            .iter()
            .filter(|resource| {
                resource
                    .audit_attribution
                    .as_ref()
                    .is_some_and(|attribution| !attribution.observations.is_empty())
            })
            .count();
    }
    let status = if statuses.len() == 1 {
        statuses
            .into_iter()
            .next()
            .unwrap_or_else(|| "NoMatch".to_string())
    } else if statuses.is_empty() {
        "NoMatch".to_string()
    } else {
        "mixed audit statuses".to_string()
    };
    format!(
        "Observed resource actors: {} principal(s), {} event(s), {} matched resource(s) | {}",
        principals.len(),
        event_ids.len(),
        matched_resources,
        status
    )
}

fn source_versions(
    left_label: &str,
    right_label: &str,
    left: Option<tfdiff::SourceVersion>,
    right: Option<tfdiff::SourceVersion>,
) -> Option<tfdiff::SourceVersions> {
    (left.is_some() || right.is_some()).then(|| tfdiff::SourceVersions {
        left_label: left_label.to_string(),
        right_label: right_label.to_string(),
        left,
        right,
    })
}

fn write_export(destination: &Path, inputs: &[PathBuf], contents: &str) -> Result<()> {
    for input in inputs {
        if destination == input {
            return Err(argument_error(
                "export destination must not be either input path",
            ));
        }
        if tfdiff::input::InputSource::is_s3_uri(input) {
            continue;
        }
        if let (Ok(destination), Ok(input)) =
            (fs::canonicalize(destination), fs::canonicalize(input))
        {
            if destination == input {
                return Err(argument_error(
                    "export destination must not be either input path",
                ));
            }
        }
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(TfdiffError::IoError)?;
    file.write_all(contents.as_bytes())
        .map_err(TfdiffError::IoError)
}

/// Writes HTML to a temp file and opens it with the platform opener.
fn open_html_in_browser(html: &str, file_prefix: &str) -> Result<()> {
    let temp_file_path = std::env::temp_dir().join(format!(
        "{file_prefix}_{}.html",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    ));
    fs::write(&temp_file_path, html).map_err(TfdiffError::IoError)?;
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(&temp_file_path).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", "start", temp_file_path.to_str().unwrap()])
            .status()
    } else {
        Command::new("xdg-open").arg(&temp_file_path).status()
    };
    if status.map_or(true, |status| !status.success()) {
        eprintln!(
            "Failed to open browser; HTML saved to {}",
            temp_file_path.display()
        );
    }
    Ok(())
}

/// Validates --compact option combinations before any AWS request is made.
/// Renders `--only-state-migrations` in the selected format.
fn render_state_migrations(cli: &Cli, report: &tfdiff::inventory::BatchReport) -> Result<()> {
    use tfdiff::migrations::*;
    let output = if cli.summary && matches!(cli.format, OutputFormat::Terminal) {
        format_migrations_summary(report)
    } else {
        match cli.format {
            OutputFormat::Terminal => format_migrations_terminal(
                report,
                resolve_color(
                    cli.color,
                    io::stdout().is_terminal() || (cli.pager && !cli.unbuffer),
                ),
            ),
            OutputFormat::Json => format_migrations_json(report)?,
            OutputFormat::Html => format_migrations_html(report),
            OutputFormat::Markdown => format_migrations_markdown(report),
        }
    };
    if cli.browser {
        open_html_in_browser(&format_migrations_html(report), "tfdiff_migrations")?;
    } else {
        write_output(
            &output,
            cli.pager,
            cli.browser,
            cli.unbuffer,
            io::stdout().is_terminal(),
        )?;
    }
    if report.counts.errors > 0 || !report.complete {
        return Err(TfdiffError::ParseError(format!(
            "S3 inventory completed with {} error(s); partial results were rendered",
            report.counts.errors
        )));
    }
    Ok(())
}

async fn run_tfstate_duplicates(
    cli: &Cli,
    paths: &[PathBuf],
    cache_policy: &tfdiff::input::S3CachePolicy,
    s3_config: &tfdiff::input::S3ClientConfig,
) -> Result<()> {
    use tfdiff::input::InputSource;
    const SHAPE: &str = "--show-tfstate-duplicates requires exactly one S3 prefix or glob input";
    if paths.len() != 1 {
        return Err(argument_error(SHAPE));
    }
    let mode = cli.pattern_mode_for(&paths[0].to_string_lossy());
    let InputSource::S3Inventory(source) = InputSource::parse_with_mode(&paths[0], mode)? else {
        return Err(argument_error(SHAPE));
    };
    if !matches!(cli.format, OutputFormat::Terminal | OutputFormat::Json) {
        return Err(argument_error(
            "--show-tfstate-duplicates supports --format terminal or json",
        ));
    }
    let report = tfdiff::tfstate_duplicates::build_duplicate_report(
        &source,
        cache_policy,
        s3_config,
        cli.unbuffer,
        resolve_color(cli.color, io::stderr().is_terminal()),
    )
    .await?;
    let output = match cli.format {
        OutputFormat::Json => tfdiff::tfstate_branches::format_json(&report),
        _ => tfdiff::tfstate_branches::format_terminal(
            &report,
            cli.verbose,
            cli.show_datetime,
            cli.full_hash,
            chrono::Utc::now(),
        ),
    };
    write_output(
        &output,
        cli.pager,
        cli.browser,
        cli.unbuffer,
        io::stdout().is_terminal(),
    )?;
    if !report.errors.is_empty() {
        for error in &report.errors {
            eprintln!("Error: {error}");
        }
        return Err(TfdiffError::InputError {
            input: paths[0].to_string_lossy().into_owned(),
            message: format!(
                "S3 listing completed with {} error(s); partial results were rendered",
                report.errors.len()
            ),
        });
    }
    Ok(())
}

fn validate_compact_arguments(cli: &Cli, paths: &[PathBuf]) -> Result<()> {
    use tfdiff::input::{InputSource, VersionSelector};
    const SHAPE: &str = "--compact requires exactly one S3 prefix, glob, or exact object input";
    if paths.len() != 1 {
        return Err(argument_error(SHAPE));
    }
    let mode = cli.pattern_mode_for(&paths[0].to_string_lossy());
    match InputSource::parse_with_mode(&paths[0], mode)? {
        InputSource::S3Inventory(_) => {}
        InputSource::S3(object) => {
            if object.selector != VersionSelector::Current {
                return Err(argument_error(
                    "--compact cannot be combined with an explicit S3 version selector; use --since/--until",
                ));
            }
        }
        InputSource::Local(_) => return Err(argument_error(SHAPE)),
    }
    if cli.skip_deleted || cli.full_inventory || cli.sort_order.is_some() {
        return Err(argument_error(
            "--compact cannot be combined with --skip-deleted, --full-inventory, or --sort-order",
        ));
    }
    if cli.show_authors || cli.show_atomic_authors {
        return Err(argument_error(
            "--compact does not support --show-authors or --show-atomic-authors",
        ));
    }
    if cli.generate_tfplan_file.is_some() {
        return Err(argument_error(
            "--compact cannot be combined with --generate-tfplan-file",
        ));
    }
    Ok(())
}

fn argument_error(message: &str) -> TfdiffError {
    TfdiffError::ParseError(format!("Invalid command line arguments: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn author_options_parse_and_require_a_visibility_flag() {
        let defaults = Cli::try_parse_from(["tfdiff", "older.tfstate", "later.tfstate"]).unwrap();
        assert!(!defaults.show_authors);
        assert!(defaults.author_window.is_none());

        let both = Cli::try_parse_from([
            "tfdiff",
            "older.tfstate",
            "later.tfstate",
            "--show-authors",
            "--show-atomic-authors",
            "--author-region",
            "eu-central-1",
            "--author-region",
            "us-east-1",
            "--author-window",
            "10m",
        ])
        .unwrap();
        assert!(both.show_authors && both.show_atomic_authors);
        assert_eq!(both.author_regions, ["eu-central-1", "us-east-1"]);
        assert_eq!(
            tfdiff::authors::parse_author_window(both.author_window.as_deref().unwrap())
                .unwrap()
                .as_secs(),
            600
        );
        assert!(Cli::try_parse_from(["tfdiff", "--author-window", "25h"]).is_ok());
        assert!(Cli::try_parse_from(["tfdiff", "--author-region", "eu-central-1"]).is_ok());
        assert!(Cli::try_parse_from([
            "tfdiff",
            "--author-log-group",
            "arn:aws:logs:eu-central-1:111122223333:log-group:example"
        ])
        .is_ok());

        let invalid_without_flag = tfdiff::authors::AuthorOptions {
            show_authors: false,
            show_atomic_authors: false,
            page_size: tfdiff::authors::DEFAULT_CLOUDTRAIL_PAGE_SIZE,
            regions: vec!["eu-central-1".to_string()],
            log_groups: Vec::new(),
            window: tfdiff::authors::DEFAULT_AUTHOR_WINDOW,
            iam_region: None,
            global_region: None,
            auxiliary_options_provided: true,
        };
        assert!(invalid_without_flag.validate().is_err());
    }

    #[test]
    fn author_source_validation_accepts_history_inventory_and_right_side_s3() {
        let s3 =
            tfdiff::input::InputSource::parse(std::path::Path::new("s3://bucket/later.tfstate"))
                .unwrap();
        let local =
            tfdiff::input::InputSource::parse(std::path::Path::new("later.tfstate")).unwrap();
        let inventory = tfdiff::input::InputSource::parse(std::path::Path::new(
            "s3://bucket/states/**/*.tfstate",
        ))
        .unwrap();
        assert!(validate_author_source_shape(&[local.clone(), s3.clone()], false).is_ok());
        assert!(validate_author_source_shape(std::slice::from_ref(&s3), false).is_ok());
        assert!(validate_author_source_shape(&[local.clone(), local], false).is_err());
        assert!(validate_author_source_shape(std::slice::from_ref(&inventory), true).is_ok());
        assert!(validate_author_source_shape(&[], false).is_err());
    }

    #[test]
    fn author_notice_flag_detection_matches_only_option_tokens() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert!(author_flag_requested(&args(&["tfdiff", "--show-authors"])));
        assert!(author_flag_requested(&args(&[
            "tfdiff",
            "--show-atomic-authors=true"
        ])));
        assert!(!author_flag_requested(&args(&[
            "tfdiff",
            "--",
            "--show-authors"
        ])));
        assert!(!author_flag_requested(&args(&[
            "tfdiff",
            "--format",
            "--show-authors"
        ])));
        assert!(!author_flag_requested(&args(&[
            "tfdiff",
            "--aws-iam-region",
            "us-west-2"
        ])));
        assert!(!author_flag_requested(&args(&[
            "tfdiff",
            "--aws-global-region=ap-south-1"
        ])));
        assert!(!author_flag_requested(&args(&[
            "tfdiff",
            "--aws-iam-region",
            "--show-authors"
        ])));
        assert!(author_flag_requested(&args(&[
            "tfdiff",
            "--aws-iam-region",
            "us-west-2",
            "--show-authors"
        ])));
    }

    #[test]
    fn aws_global_service_region_flags_parse_before_and_after_subcommands() {
        let before = Cli::try_parse_from([
            "tfdiff",
            "--aws-iam-region",
            "us-west-2",
            "--aws-global-region",
            "ap-south-1",
            "older.tfstate",
            "later.tfstate",
        ])
        .unwrap();
        assert_eq!(before.aws_iam_region.as_deref(), Some("us-west-2"));
        assert_eq!(before.aws_global_region.as_deref(), Some("ap-south-1"));

        let after = Cli::try_parse_from([
            "tfdiff",
            "older.tfstate",
            "later.tfstate",
            "--show-authors",
            "--aws-iam-region=eu-central-1",
            "--aws-global-region",
            "eu-west-1",
        ])
        .unwrap();
        assert_eq!(after.aws_iam_region.as_deref(), Some("eu-central-1"));
        assert_eq!(after.aws_global_region.as_deref(), Some("eu-west-1"));
        assert!(after.aws_iam_region.is_some() || after.aws_global_region.is_some());

        let defaults = Cli::try_parse_from(["tfdiff", "older.tfstate", "later.tfstate"]).unwrap();
        assert!(defaults.aws_iam_region.is_none());
        assert!(defaults.aws_global_region.is_none());
    }

    #[test]
    fn batch_author_summary_counts_distinct_principals_events_and_resources() {
        let mut plan = tfdiff::TerraformPlan {
            author_attribution: Some(tfdiff::authors::AuditReport {
                window_start: "start".to_string(),
                window_end: "end".to_string(),
                regions: vec!["us-east-1".to_string()],
                show_atomic_authors: false,
                status: tfdiff::authors::AuditStatus::Complete,
                message: None,
                observations: vec![test_observation(
                    "event-1",
                    "arn:aws:iam::111122223333:user/a",
                )],
            }),
            ..Default::default()
        };
        plan.resources.push(tfdiff::Resource {
            id: "aws_vpc_endpoint.test".to_string(),
            plan_delta: None,
            name: "test".to_string(),
            type_name: "aws_vpc_endpoint".to_string(),
            provider: "aws".to_string(),
            action: tfdiff::ActionType::Create,
            changes: Vec::new(),
            attributes: Default::default(),
            audit_attribution: Some(tfdiff::ResourceAuditAttribution {
                status: tfdiff::authors::AuditStatus::Complete,
                observations: vec![test_observation(
                    "event-1",
                    "arn:aws:iam::111122223333:user/a",
                )],
            }),
            applied: false,
            source: None,
        });
        assert_eq!(
            format_author_summary(std::iter::once(&plan)),
            "Observed resource actors: 1 principal(s), 1 event(s), 1 matched resource(s) | Complete"
        );
    }

    fn test_observation(event_id: &str, principal_arn: &str) -> tfdiff::authors::AuditObservation {
        tfdiff::authors::AuditObservation {
            event_id: event_id.to_string(),
            event_time: "2026-10-14T00:00:00Z".to_string(),
            event_name: "CreateVpcEndpoint".to_string(),
            event_source: "ec2.amazonaws.com".to_string(),
            account_id: Some("111122223333".to_string()),
            region: Some("us-east-1".to_string()),
            principal_type: Some("IAMUser".to_string()),
            principal_arn: Some(principal_arn.to_string()),
            principal_id: None,
            username: None,
            source_identity: None,
            session_label: None,
            email_hint: None,
            identity_hint_origin: None,
            user_agent: None,
            client_hint: None,
            match_basis: "fixture".to_string(),
        }
    }

    #[test]
    fn format_shortcuts_and_pattern_mode_flags() {
        for (flag, expected) in [("--json", "Json"), ("--md", "Markdown"), ("--html", "Html")] {
            let mut cli = Cli::try_parse_from(["tfdiff", flag]).unwrap();
            cli.resolve_format_shortcuts();
            assert_eq!(format!("{:?}", cli.format), expected);
        }
        assert!(Cli::try_parse_from(["tfdiff", "--json", "--md"]).is_err());
        assert!(Cli::try_parse_from(["tfdiff", "--glob", "--regex"]).is_err());

        let glob = Cli::try_parse_from(["tfdiff", "--filter-resource-name", "aws_*"]).unwrap();
        assert_eq!(
            glob.pattern_mode_for("x"),
            tfdiff::pattern::PatternMode::Glob
        );
        assert!(!glob.resource_filter().unwrap().is_empty());
        let regex = Cli::try_parse_from([
            "tfdiff",
            "--regex",
            "--filter-resource-id",
            "logs-(a|b)",
            "--filter-resource-arn",
            "arn:.*",
        ])
        .unwrap();
        assert_eq!(
            regex.pattern_mode_for("x"),
            tfdiff::pattern::PatternMode::Regex
        );
        assert!(regex.resource_filter().is_ok());
        let invalid =
            Cli::try_parse_from(["tfdiff", "--regex", "--filter-resource-name", "aws_("]).unwrap();
        assert!(invalid.resource_filter().is_err());
    }

    #[test]
    fn pager_and_color_options_are_available_with_port_long_only() {
        let pager = Cli::try_parse_from(["tfdiff", "--pager"]).unwrap();
        assert!(pager.pager);
        let pager_short = Cli::try_parse_from(["tfdiff", "-p"]).unwrap();
        assert!(pager_short.pager);
        let port = Cli::try_parse_from(["tfdiff", "--port", "9001"]).unwrap();
        assert_eq!(port.port, 9001);
        let short_option = Cli::try_parse_from(["tfdiff", "-p", "9001"]).unwrap();
        assert!(short_option.pager);
        assert_eq!(short_option.inputs, vec![PathBuf::from("9001")]);
        for (value, expected) in [
            ("always", ColorWhen::Always),
            ("auto", ColorWhen::Auto),
            ("never", ColorWhen::Never),
        ] {
            let cli = Cli::try_parse_from(["tfdiff", "--color", value]).unwrap();
            assert_eq!(cli.color, expected);
        }
        assert!(Cli::try_parse_from(["tfdiff", "--color", "sometimes"]).is_err());
        assert!(
            Cli::try_parse_from(["tfdiff", "--unbuffer"])
                .unwrap()
                .unbuffer
        );
    }

    #[test]
    fn pager_config_prefers_tfdiff_pager_and_parses_arguments_without_shell() {
        assert_eq!(
            pager_command_from_config(Some("custom-pager -R --width '80 cols'"), Some("other")),
            Some((
                "custom-pager".to_string(),
                vec![
                    "-R".to_string(),
                    "--width".to_string(),
                    "80 cols".to_string()
                ]
            ))
        );
        assert_eq!(
            pager_command_from_config(Some(" "), Some("more -f")),
            Some(("more".to_string(), vec!["-f".to_string()]))
        );
        assert_eq!(
            pager_command_from_config(None, None),
            Some(("less".to_string(), vec!["-R".to_string()]))
        );
        assert_eq!(
            parse_command_words("pager 'single quoted' \"double quoted\" escaped\\ arg"),
            vec!["pager", "single quoted", "double quoted", "escaped arg"]
        );
    }

    #[test]
    fn color_policy_handles_tty_and_environment_precedence() {
        assert!(resolve_color_with_env(
            ColorWhen::Auto,
            true,
            false,
            None,
            false
        ));
        assert!(!resolve_color_with_env(
            ColorWhen::Auto,
            false,
            false,
            None,
            false
        ));
        assert!(resolve_color_with_env(
            ColorWhen::Auto,
            false,
            false,
            None,
            true
        ));
        assert!(!resolve_color_with_env(
            ColorWhen::Auto,
            true,
            true,
            None,
            true
        ));
        assert!(!resolve_color_with_env(
            ColorWhen::Auto,
            true,
            false,
            Some("true"),
            true
        ));
        assert!(resolve_color_with_env(
            ColorWhen::Auto,
            false,
            false,
            None,
            true
        ));
        assert!(resolve_color_with_env(
            ColorWhen::Always,
            false,
            true,
            Some("1"),
            false
        ));
        assert!(!resolve_color_with_env(
            ColorWhen::Never,
            true,
            false,
            None,
            true
        ));
    }

    #[test]
    fn pager_routes_only_for_terminal_non_browser_buffered_output_and_falls_back_on_spawn_failure()
    {
        let pager = || Some(("/nonexistent/tfdiff-pager".to_string(), Vec::new()));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_output_with(
            "report",
            OutputRouting {
                use_pager: true,
                browser_mode: false,
                unbuffer: false,
                stdout_is_terminal: false,
                pager: pager(),
            },
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(stdout, b"report\n");
        assert!(stderr.is_empty());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_output_with(
            "report",
            OutputRouting {
                use_pager: true,
                browser_mode: true,
                unbuffer: false,
                stdout_is_terminal: true,
                pager: pager(),
            },
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(stdout, b"report\n");
        assert!(stderr.is_empty());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_output_with(
            "report",
            OutputRouting {
                use_pager: true,
                browser_mode: false,
                unbuffer: true,
                stdout_is_terminal: true,
                pager: pager(),
            },
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(stdout, b"report\n");
        assert!(stderr.is_empty());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_output_with(
            "report",
            OutputRouting {
                use_pager: true,
                browser_mode: false,
                unbuffer: false,
                stdout_is_terminal: true,
                pager: pager(),
            },
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(stdout, b"report\n");
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("could not start pager /nonexistent/tfdiff-pager"));
    }

    #[test]
    fn stdout_writer_flushes_each_line_only_when_unbuffered_and_adds_one_newline() {
        #[derive(Default)]
        struct FlushWriter {
            output: Vec<u8>,
            flushes: usize,
        }
        impl Write for FlushWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut buffered = FlushWriter::default();
        write_stdout(&mut buffered, "first\nsecond", false).unwrap();
        assert_eq!(buffered.output, b"first\nsecond\n");
        assert_eq!(buffered.flushes, 1);

        let mut unbuffered = FlushWriter::default();
        write_stdout(&mut unbuffered, "first\nsecond", true).unwrap();
        assert_eq!(unbuffered.output, b"first\nsecond\n");
        assert_eq!(unbuffered.flushes, 2);

        let mut trailing_newline = FlushWriter::default();
        write_stdout(&mut trailing_newline, "one\n", true).unwrap();
        assert_eq!(trailing_newline.output, b"one\n");
        assert_eq!(trailing_newline.flushes, 1);
    }

    #[test]
    fn verify_cli() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sort_by_action_orders_action_type_name_then_id() {
        let mut plan = tfdiff::TerraformPlan::default();
        for (id, name, type_name, action) in [
            (
                "module.a.aws_zeta.alpha",
                "alpha",
                "zeta",
                tfdiff::ActionType::Create,
            ),
            (
                "module.z.aws_alpha.zulu",
                "zulu",
                "alpha",
                tfdiff::ActionType::Create,
            ),
            (
                "module.a.aws_alpha.zulu",
                "zulu",
                "alpha",
                tfdiff::ActionType::Create,
            ),
            (
                "module.z.aws_alpha.alpha",
                "alpha",
                "alpha",
                tfdiff::ActionType::Create,
            ),
            ("z.noop", "noop", "test_resource", tfdiff::ActionType::NoOp),
            ("z.read", "read", "test_resource", tfdiff::ActionType::Read),
            (
                "z.destroy",
                "destroy",
                "test_resource",
                tfdiff::ActionType::Destroy,
            ),
            (
                "z.update",
                "update",
                "test_resource",
                tfdiff::ActionType::Update,
            ),
        ] {
            plan.resources.push(tfdiff::Resource {
                id: id.to_string(),
                plan_delta: None,
                name: name.to_string(),
                type_name: type_name.to_string(),
                provider: "test".to_string(),
                action,
                changes: Vec::new(),
                attributes: Default::default(),
                audit_attribution: None,
                applied: false,
                source: None,
            });
        }

        sort_resources(&mut plan, Some(SortBy::Action));

        let sorted: Vec<_> = plan
            .resources
            .iter()
            .map(|resource| {
                (
                    resource.action.clone(),
                    resource.type_name.as_str(),
                    resource.name.as_str(),
                    resource.id.as_str(),
                )
            })
            .collect();
        assert_eq!(
            sorted,
            vec![
                (
                    tfdiff::ActionType::Create,
                    "alpha",
                    "alpha",
                    "module.z.aws_alpha.alpha"
                ),
                (
                    tfdiff::ActionType::Create,
                    "alpha",
                    "zulu",
                    "module.a.aws_alpha.zulu"
                ),
                (
                    tfdiff::ActionType::Create,
                    "alpha",
                    "zulu",
                    "module.z.aws_alpha.zulu"
                ),
                (
                    tfdiff::ActionType::Create,
                    "zeta",
                    "alpha",
                    "module.a.aws_zeta.alpha"
                ),
                (
                    tfdiff::ActionType::Update,
                    "test_resource",
                    "update",
                    "z.update"
                ),
                (
                    tfdiff::ActionType::Destroy,
                    "test_resource",
                    "destroy",
                    "z.destroy"
                ),
                (tfdiff::ActionType::Read, "test_resource", "read", "z.read"),
                (tfdiff::ActionType::NoOp, "test_resource", "noop", "z.noop"),
            ]
        );
    }

    #[test]
    fn no_sort_by_preserves_resource_order() {
        let mut plan = tfdiff::TerraformPlan::default();
        for id in ["z.resource", "a.resource"] {
            plan.resources.push(tfdiff::Resource {
                id: id.to_string(),
                plan_delta: None,
                name: id.to_string(),
                type_name: "test_resource".to_string(),
                provider: "test".to_string(),
                action: tfdiff::ActionType::Create,
                changes: Vec::new(),
                attributes: Default::default(),
                audit_attribution: None,
                applied: false,
                source: None,
            });
        }

        sort_resources(&mut plan, None);

        assert_eq!(
            plan.resources
                .iter()
                .map(|resource| resource.id.as_str())
                .collect::<Vec<_>>(),
            vec!["z.resource", "a.resource"]
        );
    }

    #[test]
    fn sort_by_action_argument_is_global_before_and_after_diff() {
        for arguments in [
            vec![
                "tfdiff",
                "--sort-by",
                "action",
                "diff",
                "older.tfstate",
                "later.tfstate",
            ],
            vec![
                "tfdiff",
                "diff",
                "older.tfstate",
                "later.tfstate",
                "--sort-by",
                "action",
            ],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert_eq!(cli.sort_by, Some(SortBy::Action));
        }
    }
}
