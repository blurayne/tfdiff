use clap::{CommandFactory, Parser, Subcommand};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tfdiff::formatter::{
    format_html_output, format_json_output, format_markdown_output, format_terminal_output,
};
use tfdiff::{compare_states, parse_terraform_output, Result, TfdiffError};

#[derive(Parser)]
#[command(name = "tfdiff")]
#[command(about = "Beautiful Terraform plan output formatter and state snapshot differ")]
#[command(version)]
struct Cli {
    /// Input file (reads from stdin if not provided); add a second path to compare states
    #[arg(value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Output format
    #[arg(short, long, value_enum, default_value = "terminal", global = true)]
    format: OutputFormat,

    /// Filter by action type
    #[arg(short = 'F', long, value_delimiter = ',', global = true)]
    filter: Vec<String>,

    /// Sort by action (create, update, destroy, read, noop), then resource type, name, and ID
    #[arg(long, value_enum, global = true)]
    sort_by: Option<SortBy>,

    /// Launch web UI
    #[arg(short, long, global = true)]
    web: bool,

    /// Port for web UI
    #[arg(short, long, default_value = "8080", global = true)]
    port: u16,

    /// Watch file for changes
    #[arg(short = 'W', long, global = true)]
    watch: bool,

    /// Verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Quiet mode (minimal output)
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Show summary only
    #[arg(short, long, global = true)]
    summary: bool,

    /// Generate HTML and open in browser
    #[arg(short, long, global = true)]
    browser: bool,

    /// Write redacted plan-shaped JSON comparison to a new file
    #[arg(long, value_name = "PATH", global = true)]
    generate_tfplan_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Explicitly compare two Terraform state snapshots
    Diff {
        /// Older (before) state snapshot
        older: PathBuf,
        /// Later (after) state snapshot
        later: PathBuf,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum OutputFormat {
    Terminal,
    Json,
    Html,
    Markdown,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum SortBy {
    Action,
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

fn action_sort_priority(action: &tfdiff::ActionType) -> u8 {
    match action {
        tfdiff::ActionType::Create => 0,
        tfdiff::ActionType::Update => 1,
        tfdiff::ActionType::Destroy => 2,
        tfdiff::ActionType::Read => 3,
        tfdiff::ActionType::NoOp => 4,
    }
}

fn open_in_browser(plan: &tfdiff::TerraformPlan) -> Result<()> {
    let html_content = format_html_output(plan);
    let temp_dir = std::env::temp_dir();
    let temp_file_path = temp_dir.join(format!(
        "tfdiff_{}.html",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
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

fn main() -> Result<()> {
    if io::stdin().is_terminal() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }

    let cli = Cli::parse();
    let (paths, state_diff) = match cli.command {
        Some(Commands::Diff { older, later }) => {
            if !cli.inputs.is_empty() {
                return Err(argument_error(
                    "diff subcommand cannot be mixed with positional input paths",
                ));
            }
            (vec![older, later], true)
        }
        None => {
            if cli.inputs.len() > 2 {
                return Err(argument_error(
                    "at most two positional input paths are accepted",
                ));
            }
            (cli.inputs.clone(), cli.inputs.len() == 2)
        }
    };
    if cli.generate_tfplan_file.is_some() && !state_diff {
        return Err(argument_error(
            "--generate-tfplan-file requires two state snapshot paths",
        ));
    }

    let mut export_json = None;
    let plan = if state_diff {
        let older = read_file(&paths[0])?;
        let later = read_file(&paths[1])?;
        let comparison = compare_states(&older, &later)?;
        if let Some(destination) = &cli.generate_tfplan_file {
            write_export(
                destination,
                &paths,
                &tfdiff::format_state_diff_export(&comparison.export)?,
            )?;
            export_json = Some(destination.display().to_string());
        }
        comparison.plan
    } else {
        let input = if let Some(path) = paths.first() {
            read_file(path)?
        } else {
            let mut buffer = String::new();
            io::stdin()
                .read_to_string(&mut buffer)
                .map_err(TfdiffError::IoError)?;
            buffer
        };
        parse_terraform_output(&input)?
    };

    if cli.web {
        eprintln!("Web UI mode on port {} (not yet implemented)", cli.port);
        return Ok(());
    }

    let mut filtered_plan = plan.clone();
    if !cli.filter.is_empty() {
        filtered_plan.resources.retain(|resource| {
            let action = match resource.action {
                tfdiff::ActionType::Create => "create",
                tfdiff::ActionType::Update => "update",
                tfdiff::ActionType::Destroy => "destroy",
                tfdiff::ActionType::Read => "read",
                tfdiff::ActionType::NoOp => "noop",
            };
            cli.filter.iter().any(|filter| filter == action)
        });
        if filtered_plan.mode == tfdiff::PlanMode::StateDiff {
            filtered_plan.summary.add = filtered_plan
                .resources
                .iter()
                .filter(|r| r.action == tfdiff::ActionType::Create)
                .count();
            filtered_plan.summary.change = filtered_plan
                .resources
                .iter()
                .filter(|r| r.action == tfdiff::ActionType::Update)
                .count();
            filtered_plan.summary.destroy = filtered_plan
                .resources
                .iter()
                .filter(|r| r.action == tfdiff::ActionType::Destroy)
                .count();
        }
    }
    sort_resources(&mut filtered_plan, cli.sort_by);

    let output = match cli.format {
        OutputFormat::Terminal => format_terminal_output(&filtered_plan),
        OutputFormat::Json => format_json_output(&filtered_plan)?,
        OutputFormat::Html => format_html_output(&filtered_plan),
        OutputFormat::Markdown => format_markdown_output(&filtered_plan),
    };
    if cli.browser {
        return open_in_browser(&filtered_plan);
    }
    if cli.summary && matches!(cli.format, OutputFormat::Terminal) {
        if filtered_plan.mode == tfdiff::PlanMode::StateDiff {
            println!(
                "Observed differences: {} added, {} changed, {} removed",
                filtered_plan.summary.add,
                filtered_plan.summary.change,
                filtered_plan.summary.destroy
            );
        } else {
            println!(
                "Summary: {} to add, {} to change, {} to destroy",
                filtered_plan.summary.add,
                filtered_plan.summary.change,
                filtered_plan.summary.destroy
            );
        }
    } else {
        println!("{output}");
    }
    if let Some(path) = export_json {
        eprintln!("Wrote redacted, partial Terraform-show-like comparison JSON to {path}; this is not a state file or applyable plan.");
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(TfdiffError::IoError)
}

fn write_export(destination: &Path, inputs: &[PathBuf], contents: &str) -> Result<()> {
    for input in inputs {
        if destination == input {
            return Err(argument_error(
                "export destination must not be either input path",
            ));
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

fn argument_error(message: &str) -> TfdiffError {
    TfdiffError::ParseError(format!("Invalid command line arguments: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
                name: name.to_string(),
                type_name: type_name.to_string(),
                provider: "test".to_string(),
                action,
                changes: Vec::new(),
                attributes: Default::default(),
                applied: false,
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
                name: id.to_string(),
                type_name: "test_resource".to_string(),
                provider: "test".to_string(),
                action: tfdiff::ActionType::Create,
                changes: Vec::new(),
                attributes: Default::default(),
                applied: false,
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
