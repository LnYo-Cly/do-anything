use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use do_anything::compare as run_compare;
use do_anything::core::bundle::{self, FailureBundles};
use do_anything::core::case::{self, CaseError, LoadedCase};
use do_anything::core::events::EventBus;
use do_anything::core::filter::Filters;
use do_anything::core::output::{self, Format};
use do_anything::core::registry::Registry;
use do_anything::core::repeat::{RepeatOutcome, RepeatReport};
use do_anything::core::report::RunOutcome;
use do_anything::core::runner;
use do_anything::core::schema;
use do_anything::core::secrets::Secrets;
use do_anything::core::strength::{self, StrengthCheck};
use do_anything::drivers;
use do_anything::exit::ExitCode as DaExitCode;
use do_anything::report as html_report;
use do_anything::session::export::ExportOptions;
use do_anything::session::model_hook::{DEFAULT_HOOK_TIMEOUT_MS, ModelHookSpec};
use do_anything::session::{
    self, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_SESSIONS_DIR, JudgeRequest, LocateRequest,
    SelectRefAction, SelectRefRequest, SessionRequest, StepInput,
};

/// An agent-friendly, cross-layer testing tool.
#[derive(Debug, Parser)]
#[command(name = "da", version, about)]
struct Cli {
    /// How command results are reported.
    #[arg(long, global = true, value_enum)]
    output: Option<OutputFormat>,

    #[command(subcommand)]
    command: Command,
}

/// Output format selected with `--output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Human-readable results on standard output, diagnostics on standard error.
    Text,
    /// A single JSON object on standard output.
    Json,
}

impl From<OutputFormat> for Format {
    fn from(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Text => Self::Text,
            OutputFormat::Json => Self::Json,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run test cases from the given files or directories.
    Run {
        /// Case files or directories to run.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Overrides a case variable. Repeat for more than one.
        #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var)]
        vars: Vec<(String, String)>,
        /// Reads variables from a YAML mapping. Repeat for more than one; a
        /// later file overrides an earlier one.
        #[arg(long = "vars-file", value_name = "FILE")]
        vars_files: Vec<PathBuf>,
        /// Only runs cases in one of these projects. Repeat for more than one.
        #[arg(long = "project", value_name = "NAME")]
        projects: Vec<String>,
        /// Only runs cases in one of these suites. Repeat for more than one.
        #[arg(long = "suite", value_name = "NAME")]
        suites: Vec<String>,
        /// Only runs cases in one of these layers. Repeat for more than one.
        #[arg(long = "layer", value_name = "NAME")]
        layers: Vec<String>,
        /// Only runs cases that have one of these tags. Repeat for more than one.
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Skips cases that have any of these tags. Repeat for more than one.
        #[arg(long = "skip-tag", value_name = "TAG")]
        skip_tags: Vec<String>,
        /// Directory that receives one subdirectory per failed case.
        #[arg(long, value_name = "DIRECTORY")]
        bundle_dir: Option<PathBuf>,
        /// Do not write failure bundles.
        #[arg(long)]
        no_bundle: bool,
        /// Write the evidence of every executed case into this empty directory.
        #[arg(long, value_name = "DIRECTORY")]
        record: Option<PathBuf>,
        /// Capture per-step session evidence (such as screenshots and DOM snapshots) under `trace/`. Requires `--record`.
        #[arg(long)]
        trace_steps: bool,
        /// Runs up to N cases at the same time. Defaults to 1.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_jobs)]
        jobs: usize,
        /// Executes each selected case N times to classify flaky cases. Defaults to 1.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_repeat)]
        repeat: usize,
    },
    /// Compare two recorded evidence directories and classify regressions.
    Compare {
        /// Baseline directory written by `da run --record`.
        #[arg(value_name = "BASELINE_DIR")]
        baseline_dir: PathBuf,
        /// Candidate directory written by `da run --record`.
        #[arg(value_name = "CANDIDATE_DIR")]
        candidate_dir: PathBuf,
    },
    /// Render a recorded evidence directory as one static HTML file.
    Report {
        /// Directory written by `da run --record`, or one `.da/failures/<run>`.
        #[arg(value_name = "DIRECTORY")]
        directory: PathBuf,
        /// Write the report here instead of `<directory>/report.html`.
        #[arg(short = 'o', value_name = "FILE")]
        output_file: Option<PathBuf>,
        /// Do not embed screenshots.
        #[arg(long)]
        shareable: bool,
    },
    /// Validate case files without running them.
    Lint {
        /// Case files or directories to validate.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Defines a variable for every case. Repeat for more than one.
        #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var)]
        vars: Vec<(String, String)>,
        /// Reads variables from a YAML mapping. Repeat for more than one; a
        /// later file overrides an earlier one.
        #[arg(long = "vars-file", value_name = "FILE")]
        vars_files: Vec<PathBuf>,
        /// Baseline case file or directory to guard against Goodhart assertion weakening.
        #[arg(long, value_name = "PATH")]
        against: Option<PathBuf>,
        /// Explicitly acknowledge assertion weakening with a non-empty reason.
        #[arg(long, value_name = "REASON")]
        allow_weaken: Option<String>,
        /// Active project filter when checking classification exclusions with `--against`.
        #[arg(long = "project", value_name = "NAME")]
        projects: Vec<String>,
        /// Active suite filter when checking classification exclusions with `--against`.
        #[arg(long = "suite", value_name = "NAME")]
        suites: Vec<String>,
        /// Active layer filter when checking classification exclusions with `--against`.
        #[arg(long = "layer", value_name = "NAME")]
        layers: Vec<String>,
        /// Active tag filter when checking tag exclusions with `--against`.
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Active skip-tag filter when checking tag exclusions with `--against`.
        #[arg(long = "skip-tag", value_name = "TAG")]
        skip_tags: Vec<String>,
    },
    /// Print the JSON Schema for the case file format.
    Schema,
    /// Serve the Model Context Protocol on standard input and output.
    #[cfg(feature = "mcp")]
    Mcp {
        /// Directory that stores session state. Defaults to `.da/sessions`.
        #[arg(long, value_name = "DIR")]
        sessions_dir: Option<PathBuf>,
        /// Idle timeout in milliseconds before sessions are reaped. Defaults to 1800000.
        #[arg(long, value_name = "MS")]
        idle_timeout_ms: Option<u64>,
        /// Optional external JSON-RPC 2.0 model hook executable exposed to MCP clients.
        #[arg(long, value_name = "EXECUTABLE")]
        model_hook: Option<String>,
        /// Optional argument passed to `--model-hook`. Repeat for more than one.
        #[arg(
            long = "model-hook-arg",
            value_name = "ARG",
            allow_hyphen_values = true
        )]
        model_hook_args: Vec<String>,
    },
    /// Manage persistent interactive sessions for step-by-step exploration.
    Session {
        /// Directory that stores session state. Defaults to `.da/sessions`.
        #[arg(long, global = true, value_name = "DIR")]
        sessions_dir: Option<PathBuf>,
        /// Idle timeout in milliseconds before sessions are reaped. Defaults to 1800000.
        #[arg(long, global = true, value_name = "MS")]
        idle_timeout_ms: Option<u64>,
        #[command(subcommand)]
        action: SessionCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Create a new interactive session directory.
    Start {
        /// Attach a declared observer (`NAME=KIND`, `NAME=KIND:key=val`, or YAML/JSON mapping). Repeat for more than one.
        #[arg(long = "observe", value_name = "SPEC")]
        observe: Vec<String>,
        /// Configure an external JSON-RPC 2.0 model hook executable for `select-ref`, `locate`, and `judge`.
        #[arg(long, value_name = "EXECUTABLE")]
        model_hook: Option<String>,
        /// Pass an argument to `--model-hook`. Repeat for more than one.
        #[arg(
            long = "model-hook-arg",
            value_name = "ARG",
            allow_hyphen_values = true
        )]
        model_hook_args: Vec<String>,
    },
    /// Execute one step in an existing session.
    Step {
        /// Session identifier returned by `da session start`.
        id: String,
        /// Step name (for example `browser.open`), when passing `--arg` flags.
        action: Option<String>,
        /// Step name (alternative to positional action).
        #[arg(long = "use", value_name = "STEP")]
        use_step: Option<String>,
        /// Step argument as `KEY=VALUE`. Repeat for more than one.
        #[arg(long = "arg", value_name = "KEY=VALUE", value_parser = parse_var)]
        args: Vec<(String, String)>,
        /// Step arguments mapping as YAML or JSON.
        #[arg(long, value_name = "YAML_OR_JSON")]
        with: Option<String>,
        /// Complete step definition as YAML or JSON.
        #[arg(long, value_name = "YAML_OR_JSON")]
        step: Option<String>,
        /// Read the step definition from a YAML or JSON file (`-` for stdin).
        #[arg(short = 'f', long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Save a value from the step result as `NAME=JSONPATH`.
        #[arg(long = "save", value_name = "NAME=JSONPATH", value_parser = parse_var)]
        save: Vec<(String, String)>,
    },
    /// Take an accessibility snapshot without executing a step.
    Snapshot {
        /// Session identifier returned by `da session start`.
        id: String,
        /// Optional element reference from a prior snapshot to expand within.
        #[arg(long = "ref", value_name = "REF")]
        scope_ref: Option<String>,
        /// Maximum traversal depth (relative to root or `--ref`).
        #[arg(long, value_name = "N")]
        max_depth: Option<usize>,
        /// Maximum nodes included before truncation.
        #[arg(long, value_name = "N")]
        max_nodes: Option<usize>,
    },
    /// Choose a candidate `ref` from the active snapshot using a `model/select_ref` JSON-RPC hook.
    SelectRef {
        /// Session identifier returned by `da session start`.
        id: String,
        /// Natural-language target description (redacted via `Secrets`).
        #[arg(long, value_name = "TEXT")]
        query: String,
        /// Optional element reference from a prior snapshot to scope candidates within.
        #[arg(long = "ref", value_name = "REF")]
        scope_ref: Option<String>,
        /// Maximum snapshot traversal depth.
        #[arg(long, value_name = "N")]
        max_depth: Option<usize>,
        /// Maximum snapshot nodes included before truncation.
        #[arg(long, value_name = "N")]
        max_nodes: Option<usize>,
        /// Override the session's model hook executable for this call.
        #[arg(long, value_name = "EXECUTABLE")]
        hook: Option<String>,
        /// Pass an argument to `--hook`. Repeat for more than one.
        #[arg(long = "hook-arg", value_name = "ARG", allow_hyphen_values = true)]
        hook_args: Vec<String>,
        /// Maximum hook runtime in milliseconds. Defaults to 30000.
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_HOOK_TIMEOUT_MS)]
        timeout_ms: u64,
        /// Immediately execute a step (such as `browser.click`) using the selected `ref`.
        #[arg(long, value_name = "STEP")]
        act: Option<String>,
        /// Step arguments mapping as YAML or JSON when `--act` is passed.
        #[arg(long, value_name = "YAML_OR_JSON")]
        with: Option<String>,
        /// Step argument as `KEY=VALUE` when `--act` is passed. Repeat for more than one.
        #[arg(long = "arg", value_name = "KEY=VALUE", value_parser = parse_var)]
        args: Vec<(String, String)>,
        /// Save a value from the `--act` step result as `NAME=JSONPATH`.
        #[arg(long = "save", value_name = "NAME=JSONPATH", value_parser = parse_var)]
        save: Vec<(String, String)>,
    },
    /// Capture an input-masked screenshot, invoke `model/locate`, and reverse-synthesize a deterministic locator.
    Locate {
        /// Session identifier returned by `da session start`.
        id: String,
        /// Visual grounding query (redacted via `Secrets`).
        #[arg(long, value_name = "TEXT")]
        query: String,
        /// Override the session's model hook executable for this call.
        #[arg(long, value_name = "EXECUTABLE")]
        hook: Option<String>,
        /// Pass an argument to `--hook`. Repeat for more than one.
        #[arg(long = "hook-arg", value_name = "ARG", allow_hyphen_values = true)]
        hook_args: Vec<String>,
        /// Maximum hook runtime in milliseconds. Defaults to 30000.
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_HOOK_TIMEOUT_MS)]
        timeout_ms: u64,
        /// Immediately click the located target and record the step for `da session export`.
        #[arg(long)]
        click: bool,
    },
    /// Invoke `model/judge` over redacted session evidence and exit 1 if `passed` is false.
    Judge {
        /// Session identifier returned by `da session start`.
        id: String,
        /// Evaluation rubric (redacted via `Secrets`).
        #[arg(long, value_name = "TEXT")]
        rubric: String,
        /// Custom evidence as YAML or JSON; defaults to redacted session steps and variables.
        #[arg(long, value_name = "YAML_OR_JSON")]
        evidence: Option<String>,
        /// Override the session's model hook executable for this call.
        #[arg(long, value_name = "EXECUTABLE")]
        hook: Option<String>,
        /// Pass an argument to `--hook`. Repeat for more than one.
        #[arg(long = "hook-arg", value_name = "ARG", allow_hyphen_values = true)]
        hook_args: Vec<String>,
        /// Maximum hook runtime in milliseconds. Defaults to 30000.
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_HOOK_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Export successful steps as a deterministic, replayable YAML case.
    Export {
        /// Session identifier returned by `da session start`.
        id: String,
        /// Write the exported YAML case to `FILE`.
        #[arg(short = 'o', long = "out", value_name = "FILE")]
        out: Option<PathBuf>,
        /// Case `name:` written to the exported YAML.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Include read-only `browser.read` steps without `expect` or `save`.
        #[arg(long)]
        include_reads: bool,
        /// Print raw YAML to stdout instead of a JSON report object.
        #[arg(long)]
        yaml: bool,
    },
    /// Close all resources and delete the session directory.
    Stop {
        /// Session identifier returned by `da session start`.
        id: String,
    },
    /// List active sessions and reap idle ones.
    List,
}

/// Splits a `--var name=value` argument.
fn parse_var(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once('=') else {
        return Err(format!("`{text}` has no `=`; write `--var name=value`"));
    };
    if name.is_empty() {
        return Err(format!(
            "the name is empty in `{text}`; write `--var name=value`"
        ));
    }
    Ok((name.to_string(), value.to_string()))
}

/// Reads the `--jobs` count.
///
/// Zero is refused instead of treated as "one job" or "no job at all", because
/// either reading would silently run something other than what was asked for.
fn parse_jobs(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err(
            "a run needs at least 1 job; write `--jobs 1` to run one case at a time".to_string(),
        ),
        Ok(jobs) => Ok(jobs),
        Err(_) => Err(format!(
            "`{text}` is not a number of jobs; write `--jobs N` with N at least 1"
        )),
    }
}

/// Reads the `--repeat` count, refusing zero so every repeat run executes at least once.
fn parse_repeat(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err(
            "a run needs `--repeat` of at least 1; write `--repeat 1` for a single run".to_string(),
        ),
        Ok(repeat) => Ok(repeat),
        Err(_) => Err(format!(
            "`{text}` is not a valid repeat count; write `--repeat N` with N at least 1"
        )),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let mut registry = Registry::new();
    if let Err(message) =
        drivers::register(&mut registry).and_then(|()| drivers::register_observers(&mut registry))
    {
        let _ = write_stderr(&format!("da: internal error: {message}\n"));
        return DaExitCode::Environment.into();
    }

    let default_output = cli.output.unwrap_or(OutputFormat::Text);

    match cli.command {
        Command::Run {
            paths,
            vars,
            vars_files,
            projects,
            suites,
            layers,
            tags,
            skip_tags,
            bundle_dir,
            no_bundle,
            record,
            trace_steps,
            jobs,
            repeat,
        } => {
            let sources = match VarSources::load(&vars_files, &vars) {
                Ok(sources) => sources,
                Err(message) => return usage_error(&message),
            };
            if trace_steps && record.is_none() {
                return usage_error("`--trace-steps` requires `--record <DIRECTORY>`");
            }
            if let Some(directory) = &record
                && let Err(message) = check_record_directory(directory)
            {
                return usage_error(&message);
            }
            run(
                default_output,
                &registry,
                RunOptions {
                    paths,
                    sources,
                    filters: Filters {
                        project: projects,
                        suite: suites,
                        layer: layers,
                        tag: tags,
                        skip_tag: skip_tags,
                    },
                    bundle_dir,
                    no_bundle,
                    record,
                    trace_steps,
                    jobs,
                    repeat,
                },
            )
        }
        Command::Compare {
            baseline_dir,
            candidate_dir,
        } => compare(default_output, &baseline_dir, &candidate_dir),
        Command::Report {
            directory,
            output_file,
            shareable,
        } => report(default_output, &directory, output_file, shareable),
        Command::Lint {
            paths,
            vars,
            vars_files,
            against,
            allow_weaken,
            projects,
            suites,
            layers,
            tags,
            skip_tags,
        } => {
            if let Some(reason) = &allow_weaken {
                if reason.trim().is_empty() {
                    return lint_usage_error(
                        default_output,
                        "`--allow-weaken` requires a non-empty reason explaining the intentional contract change",
                    );
                }
                if against.is_none() {
                    return lint_usage_error(
                        default_output,
                        "`--allow-weaken` requires `--against <PATH>`",
                    );
                }
            }
            let sources = match VarSources::load(&vars_files, &vars) {
                Ok(sources) => sources,
                Err(message) => return usage_error(&message),
            };
            lint(
                &registry,
                LintOptions {
                    paths,
                    format: default_output,
                    sources,
                    against,
                    allow_weaken,
                    filters: Filters {
                        project: projects,
                        suite: suites,
                        layer: layers,
                        tag: tags,
                        skip_tag: skip_tags,
                    },
                },
            )
        }
        Command::Schema => print_schema(&registry, default_output),
        #[cfg(feature = "mcp")]
        Command::Mcp {
            sessions_dir,
            idle_timeout_ms,
            model_hook,
            model_hook_args,
        } => {
            let hook_spec = match build_cli_hook_spec(
                model_hook,
                model_hook_args,
                "--model-hook",
                "--model-hook-arg",
            ) {
                Ok(spec) => spec,
                Err(message) => return usage_error(&message),
            };
            do_anything::mcp::serve(
                &registry,
                do_anything::mcp::Options {
                    sessions_dir: sessions_dir
                        .unwrap_or_else(|| PathBuf::from(DEFAULT_SESSIONS_DIR)),
                    idle_timeout_ms: idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS),
                    model_hook: hook_spec,
                },
            )
            .into()
        }
        Command::Session {
            sessions_dir,
            idle_timeout_ms,
            action,
        } => run_session(
            cli.output.unwrap_or(OutputFormat::Json),
            &registry,
            sessions_dir,
            idle_timeout_ms,
            action,
        ),
    }
}

fn build_cli_hook_spec(
    command: Option<String>,
    args: Vec<String>,
    cmd_flag: &str,
    arg_flag: &str,
) -> Result<Option<ModelHookSpec>, String> {
    match command {
        Some(cmd) => {
            if cmd.trim().is_empty() {
                return Err(format!("`{cmd_flag}` executable path must not be empty"));
            }
            Ok(Some(ModelHookSpec { command: cmd, args }))
        }
        None if args.is_empty() => Ok(None),
        None => Err(format!("`{arg_flag}` requires `{cmd_flag} <EXECUTABLE>`")),
    }
}

fn run_session(
    format: OutputFormat,
    registry: &Registry,
    sessions_dir: Option<PathBuf>,
    idle_timeout_ms: Option<u64>,
    action: SessionCommand,
) -> ExitCode {
    let sessions_dir = sessions_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_SESSIONS_DIR));
    let mut print_raw_yaml = false;

    let request = match action {
        SessionCommand::Start {
            observe,
            model_hook,
            model_hook_args,
        } => {
            let parsed_observe = match session::parse_observe_specs(&observe, registry) {
                Ok(items) => items,
                Err(message) => return session_usage_error(format, &message),
            };
            let hook_spec = match build_cli_hook_spec(
                model_hook,
                model_hook_args,
                "--model-hook",
                "--model-hook-arg",
            ) {
                Ok(spec) => spec,
                Err(message) => return session_usage_error(format, &message),
            };
            SessionRequest::Start {
                sessions_dir,
                idle_timeout_ms: idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS),
                observe: parsed_observe,
                model_hook: hook_spec,
            }
        }
        SessionCommand::Step {
            id,
            action,
            use_step,
            args,
            with,
            step,
            file,
            save,
        } => {
            let step_input =
                match resolve_step_input(action, use_step, args, with, step, file, save) {
                    Ok(input) => input,
                    Err(message) => return session_usage_error(format, &message),
                };
            let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            SessionRequest::Step {
                sessions_dir,
                id,
                base_directory,
                step: step_input,
            }
        }
        SessionCommand::Snapshot {
            id,
            scope_ref,
            max_depth,
            max_nodes,
        } => SessionRequest::Snapshot {
            sessions_dir,
            id,
            scope_ref,
            max_depth,
            max_nodes,
        },
        SessionCommand::SelectRef {
            id,
            query,
            scope_ref,
            max_depth,
            max_nodes,
            hook,
            hook_args,
            timeout_ms,
            act,
            with,
            args,
            save,
        } => {
            let hook_override = match build_cli_hook_spec(hook, hook_args, "--hook", "--hook-arg") {
                Ok(spec) => spec,
                Err(message) => return session_usage_error(format, &message),
            };
            if (with.is_some() || !args.is_empty() || !save.is_empty()) && act.is_none() {
                return session_usage_error(
                    format,
                    "`--with`, `--arg`, and `--save` on `select-ref` require `--act <STEP>`",
                );
            }
            let act_spec = act.map(|action| SelectRefAction {
                action,
                with_raw: with,
                args,
                save,
            });
            let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            SessionRequest::SelectRef(SelectRefRequest {
                sessions_dir,
                id,
                base_directory,
                query,
                scope_ref,
                max_depth,
                max_nodes,
                hook_override,
                timeout_ms,
                act: act_spec,
            })
        }
        SessionCommand::Locate {
            id,
            query,
            hook,
            hook_args,
            timeout_ms,
            click,
        } => {
            let hook_override = match build_cli_hook_spec(hook, hook_args, "--hook", "--hook-arg") {
                Ok(spec) => spec,
                Err(message) => return session_usage_error(format, &message),
            };
            let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            SessionRequest::Locate(LocateRequest {
                sessions_dir,
                id,
                base_directory,
                query,
                hook_override,
                timeout_ms,
                click,
            })
        }
        SessionCommand::Judge {
            id,
            rubric,
            evidence,
            hook,
            hook_args,
            timeout_ms,
        } => {
            let hook_override = match build_cli_hook_spec(hook, hook_args, "--hook", "--hook-arg") {
                Ok(spec) => spec,
                Err(message) => return session_usage_error(format, &message),
            };
            let parsed_evidence = match evidence {
                Some(raw) => match case::parse_json_value(&raw) {
                    Ok(val) => Some(val),
                    Err(err) => {
                        return session_usage_error(
                            format,
                            &format!("could not parse `--evidence`: {err}"),
                        );
                    }
                },
                None => None,
            };
            SessionRequest::Judge(JudgeRequest {
                sessions_dir,
                id,
                rubric,
                evidence: parsed_evidence,
                hook_override,
                timeout_ms,
            })
        }
        SessionCommand::Export {
            id,
            out,
            name,
            include_reads,
            yaml,
        } => {
            print_raw_yaml = yaml || (format == OutputFormat::Text && out.is_none());
            SessionRequest::Export {
                sessions_dir,
                id,
                options: ExportOptions {
                    case_name: name,
                    include_reads,
                },
                out_path: out,
            }
        }
        SessionCommand::Stop { id } => SessionRequest::Stop { sessions_dir, id },
        SessionCommand::List => SessionRequest::List {
            sessions_dir,
            idle_override_ms: idle_timeout_ms,
        },
    };

    let response = session::handle(request, registry);
    if print_raw_yaml
        && response.exit_code == DaExitCode::Success
        && let Some(yaml) = &response.raw_yaml
    {
        let _ = write_stdout(yaml);
    } else if format == OutputFormat::Text
        && let Some(text) = &response.raw_text
    {
        let _ = write_stdout(text);
    } else if format == OutputFormat::Json {
        let _ = write_stdout(&format!("{}\n", response.payload));
    } else {
        let rendered = serde_json::to_string_pretty(&response.payload)
            .unwrap_or_else(|_| response.payload.to_string());
        let _ = write_stdout(&format!("{rendered}\n"));
    }
    response.exit_code.into()
}

fn session_usage_error(format: OutputFormat, message: &str) -> ExitCode {
    if format == OutputFormat::Json {
        let err = serde_json::json!({
            "ok": false,
            "error": { "kind": "usage", "message": message }
        });
        let _ = write_stdout(&format!("{err}\n"));
    } else {
        let _ = write_stderr(&format!("error: {message}\n"));
    }
    DaExitCode::Usage.into()
}

fn resolve_step_input(
    action: Option<String>,
    use_step: Option<String>,
    args: Vec<(String, String)>,
    with: Option<String>,
    step: Option<String>,
    file: Option<PathBuf>,
    save: Vec<(String, String)>,
) -> Result<StepInput, String> {
    let chosen_action = match (action, use_step) {
        (Some(_), Some(_)) => {
            return Err(
                "pass either positional `<action>` or `--use <step>`, not both".to_string(),
            );
        }
        (Some(a), None) => Some(a),
        (None, Some(u)) => Some(u),
        (None, None) => None,
    };

    let sources = [
        chosen_action.is_some() || !args.is_empty() || with.is_some() || !save.is_empty(),
        step.is_some(),
        file.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();

    if sources > 1 {
        return Err(
            "provide one step source: stdin, `--file`, `--step`, or `<action>`/`--use` flags"
                .to_string(),
        );
    }

    if let Some(path) = file {
        if path.as_os_str() == "-" {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("could not read step from stdin: {error}"))?;
            if text.trim().is_empty() {
                return Err("stdin was empty; pass a YAML or JSON step".to_string());
            }
            return Ok(StepInput::Text(text));
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            format!(
                "could not read step file `{}`: {error}",
                path.to_string_lossy()
            )
        })?;
        return Ok(StepInput::Text(text));
    }

    if let Some(text) = step {
        if text.trim().is_empty() {
            return Err("`--step` must not be empty".to_string());
        }
        return Ok(StepInput::Text(text));
    }

    if let Some(action) = chosen_action {
        return Ok(StepInput::Flags {
            action,
            with_raw: with,
            args,
            save,
        });
    }

    if !args.is_empty() || with.is_some() || !save.is_empty() {
        return Err(
            "`--arg`, `--with`, and `--save` require a step name (`<action>` or `--use`)"
                .to_string(),
        );
    }

    if io::stdin().is_terminal() {
        return Err(
            "no step provided; pipe a YAML/JSON step on stdin, pass `--file`, `--step`, or `<action> --arg key=value`"
                .to_string(),
        );
    }
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("could not read step from stdin: {error}"))?;
    if text.trim().is_empty() {
        return Err(
            "no step provided on stdin; pipe a YAML/JSON step, pass `--file`, `--step`, or `<action> --arg key=value`"
                .to_string(),
        );
    }
    Ok(StepInput::Text(text))
}

/// Variables that come from outside the case file, in increasing precedence.
#[derive(Debug, Default)]
struct VarSources {
    /// The values of each `--vars-file`, with the file they came from, in
    /// command-line order.
    files: Vec<(PathBuf, Vec<(String, String)>)>,
    /// The values of each `--var`.
    inline: Vec<(String, String)>,
}

impl VarSources {
    /// Reads every `--vars-file` in command-line order.
    ///
    /// The file each value came from is kept so that a problem inside one can
    /// name it; the values are applied in this order, so a later file wins.
    fn load(paths: &[PathBuf], inline: &[(String, String)]) -> Result<Self, String> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            files.push((path.clone(), case::load_vars_file(path)?));
        }
        Ok(Self {
            files,
            inline: inline.to_vec(),
        })
    }
}

/// Reports a problem with the command line without running anything.
fn usage_error(message: &str) -> ExitCode {
    let _ = write_stderr(&format!("error: {message}\n"));
    DaExitCode::Usage.into()
}

/// Refuses a record directory that already holds files.
///
/// Writing a second run into the same names would leave a mix of two runs, and
/// deleting the old contents could destroy evidence the user still needs.
fn check_record_directory(directory: &PathBuf) -> Result<(), String> {
    match std::fs::metadata(directory) {
        Ok(metadata) if !metadata.is_dir() => Err(format!(
            "`{}` is not a directory; choose an empty directory for `--record`",
            do_anything::core::paths::display(directory)
        )),
        Ok(_) => {
            let mut entries = std::fs::read_dir(directory).map_err(|error| {
                format!(
                    "could not read `{}`: {error}",
                    do_anything::core::paths::display(directory)
                )
            })?;
            if entries.next().is_some() {
                return Err(format!(
                    "`{}` is not empty; choose an empty directory for `--record` or remove its contents first",
                    do_anything::core::paths::display(directory)
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not check `{}`: {error}",
            do_anything::core::paths::display(directory)
        )),
    }
}

/// Everything one `da run` asked for, as the command line stated it.
struct RunOptions {
    paths: Vec<PathBuf>,
    sources: VarSources,
    filters: Filters,
    bundle_dir: Option<PathBuf>,
    no_bundle: bool,
    record: Option<PathBuf>,
    trace_steps: bool,
    jobs: usize,
    repeat: usize,
}

fn run(format: OutputFormat, registry: &Registry, options: RunOptions) -> ExitCode {
    if options.repeat <= 1 {
        let mut secrets = Secrets::new();
        let outcome = execute(registry, &options, &mut secrets);
        let rendered = output::render(&outcome, format.into(), &secrets);
        // A write error must not replace the run's exit code. The common case is a
        // reader that exits early, such as `head` or `jq`; there is usually nowhere
        // left to report the failure either.
        let _ = write_stdout(&rendered.stdout);
        let _ = write_stderr(&rendered.stderr);
        return outcome.exit_code().into();
    }

    let mut secrets = Secrets::new();
    let mut outcome = execute_repeat(registry, &options, &mut secrets);
    if let (Some(record_dir), RepeatOutcome::Completed(_)) = (&options.record, &outcome) {
        let summary_value = output::repeat_json(&outcome, &secrets);
        if let Err(message) = write_repeat_summary(record_dir, &summary_value)
            && let RepeatOutcome::Completed(report) = &mut outcome
        {
            report.bundle_errors.push(message);
        }
    }
    let rendered = output::render_repeat(&outcome, format.into(), &secrets);
    let _ = write_stdout(&rendered.stdout);
    let _ = write_stderr(&rendered.stderr);
    outcome.exit_code().into()
}

fn write_repeat_summary(
    record_dir: &std::path::Path,
    value: &serde_json::Value,
) -> Result<(), String> {
    std::fs::create_dir_all(record_dir).map_err(|error| {
        format!(
            "could not create `{}`: {error}",
            do_anything::core::paths::display(record_dir)
        )
    })?;
    let summary_path = record_dir.join("repeat-summary.json");
    let text = serde_json::to_string_pretty(value).map_err(|error| {
        format!(
            "could not serialize `{}`: {error}",
            do_anything::core::paths::display(&summary_path)
        )
    })?;
    std::fs::write(&summary_path, format!("{text}\n")).map_err(|error| {
        format!(
            "could not write `{}`: {error}",
            do_anything::core::paths::display(&summary_path)
        )
    })
}

fn execute_repeat(
    registry: &Registry,
    options: &RunOptions,
    secrets: &mut Secrets,
) -> RepeatOutcome {
    let LoadedCases { cases, errors, .. } = load_cases(&options.paths, &options.sources, secrets);
    if !errors.is_empty() {
        return RepeatOutcome::Rejected {
            repeat_count: options.repeat,
            errors,
        };
    }

    let started = Instant::now();
    let mut attempt_reports = Vec::with_capacity(options.repeat);

    for attempt in 1..=options.repeat {
        let mut bundles = if options.no_bundle {
            None
        } else {
            Some(FailureBundles::new(
                options
                    .bundle_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(bundle::DEFAULT_DIRECTORY)),
            ))
        };
        let attempt_record_dir = options
            .record
            .as_ref()
            .map(|root| root.join(format!("attempt-{attempt}")));
        let mut record = attempt_record_dir.clone().map(FailureBundles::record);

        let bus = EventBus::new();
        let outcome = runner::run_with(
            &cases,
            registry,
            &bus,
            runner::RecordWriters {
                failures: bundles.as_mut(),
                record: record.as_mut(),
                trace_steps: options.trace_steps,
            },
            &options.filters,
            secrets,
            options.jobs,
        );

        match outcome {
            RunOutcome::Rejected { errors } => {
                return RepeatOutcome::Rejected {
                    repeat_count: options.repeat,
                    errors,
                };
            }
            RunOutcome::NoMatch { message } => {
                return RepeatOutcome::NoMatch {
                    repeat_count: options.repeat,
                    message,
                };
            }
            RunOutcome::Completed(report) => {
                if let Some(dir) = &attempt_record_dir
                    && report.cases.is_empty()
                {
                    let _ = std::fs::create_dir_all(dir);
                }
                attempt_reports.push(report);
            }
        }
    }

    RepeatOutcome::Completed(RepeatReport::from_attempts(
        options.repeat,
        attempt_reports,
        started.elapsed(),
        Vec::new(),
    ))
}

fn compare(
    format: OutputFormat,
    baseline_dir: &std::path::Path,
    candidate_dir: &std::path::Path,
) -> ExitCode {
    match run_compare::compare(baseline_dir, candidate_dir) {
        Ok(report) => {
            let rendered = output::render_compare(&report, format.into());
            let _ = write_stdout(&rendered.stdout);
            let _ = write_stderr(&rendered.stderr);
            if report.is_ok() {
                DaExitCode::Success.into()
            } else {
                DaExitCode::TestFailure.into()
            }
        }
        Err(error) => {
            let rendered =
                output::render_compare_error(error.kind(), error.message(), format.into());
            let _ = write_stdout(&rendered.stdout);
            let _ = write_stderr(&rendered.stderr);
            error.exit_code().into()
        }
    }
}

/// Renders one recorded directory and reports where it was written.
fn report(
    format: OutputFormat,
    directory: &std::path::Path,
    output_file: Option<PathBuf>,
    shareable: bool,
) -> ExitCode {
    let report = match html_report::build(directory, shareable) {
        Ok(report) => report,
        Err(error) => {
            let _ = write_stderr(&format!("error: {}\n", error.message()));
            return error.exit_code().into();
        }
    };
    let path = output_file.unwrap_or_else(|| directory.join("report.html"));
    if let Err(error) = report.write(&path) {
        let _ = write_stderr(&format!("error: {}\n", error.message()));
        return error.exit_code().into();
    }
    let rendered = output::render_report(report.summary(), &path, format.into());
    let _ = write_stdout(&rendered.stdout);
    let _ = write_stderr(&rendered.stderr);
    DaExitCode::Success.into()
}

fn execute(registry: &Registry, options: &RunOptions, secrets: &mut Secrets) -> RunOutcome {
    let LoadedCases { cases, errors, .. } = load_cases(&options.paths, &options.sources, secrets);
    if !errors.is_empty() {
        return RunOutcome::Rejected { errors };
    }

    let mut bundles = if options.no_bundle {
        None
    } else {
        Some(FailureBundles::new(
            options
                .bundle_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(bundle::DEFAULT_DIRECTORY)),
        ))
    };
    let mut record = options.record.clone().map(FailureBundles::record);

    let bus = EventBus::new();
    runner::run_with(
        &cases,
        registry,
        &bus,
        runner::RecordWriters {
            failures: bundles.as_mut(),
            record: record.as_mut(),
            trace_steps: options.trace_steps,
        },
        &options.filters,
        secrets,
        options.jobs,
    )
}

struct LintOptions {
    paths: Vec<PathBuf>,
    format: OutputFormat,
    sources: VarSources,
    against: Option<PathBuf>,
    allow_weaken: Option<String>,
    filters: Filters,
}

fn lint_usage_error(format: OutputFormat, message: &str) -> ExitCode {
    if format == OutputFormat::Json {
        let err = serde_json::json!({
            "ok": false,
            "error": { "kind": "usage", "message": message }
        });
        let _ = write_stdout(&format!("{err}\n"));
    }
    let _ = write_stderr(&format!("error: {message}\n"));
    DaExitCode::Usage.into()
}

/// Validates the cases without running them.
///
/// It shares discovery, loading and [`runner::preflight`] with `da run`, so the
/// two commands cannot disagree about which cases are valid. Unlike a run, it
/// keeps going after the first bad file and reports every problem at once.
fn lint(registry: &Registry, options: LintOptions) -> ExitCode {
    let mut secrets = Secrets::new();
    let LoadedCases {
        files,
        cases,
        mut errors,
    } = load_cases(&options.paths, &options.sources, &mut secrets);
    errors.extend(runner::preflight(&cases, registry, &mut secrets));
    let warnings = runner::preflight_warnings(&cases, registry, &mut secrets);

    let mut strength_findings = Vec::new();
    if let Some(against_path) = &options.against {
        let baseline_inputs = std::slice::from_ref(against_path);
        let LoadedCases {
            cases: baseline_cases,
            errors: baseline_errors,
            ..
        } = load_cases(baseline_inputs, &options.sources, &mut secrets);
        errors.extend(baseline_errors);
        errors.extend(runner::preflight(&baseline_cases, registry, &mut secrets));

        if errors.is_empty() {
            strength_findings = strength::compare_strength(
                &StrengthCheck {
                    baseline_inputs,
                    baseline_cases: &baseline_cases,
                    candidate_inputs: &options.paths,
                    candidate_cases: &cases,
                    registry,
                    filters: &options.filters,
                    acknowledged: options.allow_weaken.is_some(),
                },
                &mut secrets,
            );
        }
    }

    let report = output::LintReport {
        files: files.len(),
        errors,
        warnings,
        cases: cases.iter().map(output::LintCase::new).collect(),
        against: options.against,
        strength_findings,
        acknowledged_reason: options.allow_weaken,
    };
    let rendered = output::render_lint(&report, options.format.into(), &secrets);
    let _ = write_stdout(&rendered.stdout);
    let _ = write_stderr(&rendered.stderr);

    if report.is_ok() {
        DaExitCode::Success.into()
    } else {
        DaExitCode::InvalidCase.into()
    }
}

/// Writes the schema of the case file format to standard output.
fn print_schema(registry: &Registry, format: OutputFormat) -> ExitCode {
    let value = schema::case_schema(registry);
    let rendered = output::render_schema(&value, format.into());
    let _ = write_stdout(&rendered.stdout);
    let _ = write_stderr(&rendered.stderr);
    DaExitCode::Success.into()
}

/// The files a path list expands to and the cases that loaded from them.
struct LoadedCases {
    files: Vec<PathBuf>,
    cases: Vec<LoadedCase>,
    errors: Vec<CaseError>,
}

/// Discovers, loads and parses every case without running anything.
///
/// A file that does not parse becomes an entry in `errors`; the remaining files
/// still load, so lint can report every problem at once.
fn load_cases(paths: &[PathBuf], sources: &VarSources, secrets: &mut Secrets) -> LoadedCases {
    let files = match case::discover(paths) {
        Ok(files) => files,
        Err(error) => {
            return LoadedCases {
                files: Vec::new(),
                cases: Vec::new(),
                errors: vec![error],
            };
        }
    };
    if files.is_empty() {
        return LoadedCases {
            files,
            cases: Vec::new(),
            errors: vec![CaseError {
                file: paths.first().cloned().unwrap_or_default(),
                message: "no `.yaml` or `.yml` case files found; pass a case file or a directory that contains one".to_string(),
            }],
        };
    }

    let mut cases = Vec::with_capacity(files.len());
    let mut errors = Vec::new();
    // The command line beats the case file, and a later source beats an earlier
    // one: `vars:` first, then each `--vars-file` in order, then `--var`.
    // A value in a vars file may itself reference the environment; the problem
    // then belongs to that file, not to the case the value ends up in.
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut overridden_names = BTreeSet::new();
    for (file, entries) in &sources.files {
        for (name, value) in entries {
            overridden_names.insert(name.clone());
            match case::expand_env(value, secrets) {
                Ok(value) => overrides.push((name.clone(), value)),
                Err(message) => errors.push(CaseError {
                    file: file.clone(),
                    message: format!("variable `{name}`: {message}"),
                }),
            }
        }
    }
    for (name, _) in &sources.inline {
        overridden_names.insert(name.clone());
    }
    overrides.extend(sources.inline.iter().cloned());
    let overridden_vars: Vec<String> = overridden_names.into_iter().collect();

    for file in &files {
        match case::load_file(file) {
            Ok(mut loaded_case) => {
                if let Err(message) = case::expand_vars(&mut loaded_case.case.vars, secrets) {
                    errors.push(CaseError {
                        file: loaded_case.path.clone(),
                        message,
                    });
                    continue;
                }
                // A case that does not use the name ignores it.
                for (name, value) in &overrides {
                    loaded_case.case.vars.insert(name.clone(), value.clone());
                }
                loaded_case.overridden_vars = overridden_vars.clone();
                cases.push(loaded_case);
            }
            Err(error) => errors.push(error),
        }
    }

    LoadedCases {
        files,
        cases,
        errors,
    }
}

/// Writes `text` to standard output, returning a write error instead of
/// panicking.
///
/// `print!` panics when the reader of the pipe exits early, which replaces the
/// run's exit code with 101 and hides the test result.
fn write_stdout(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// Writes `text` to standard error, returning a write error instead of
/// panicking for the same reason as [`write_stdout`].
fn write_stderr(text: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(text.as_bytes())?;
    stderr.flush()
}
