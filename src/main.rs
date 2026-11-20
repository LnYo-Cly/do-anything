use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use do_anything::core::bundle::{self, FailureBundles};
use do_anything::core::case::{self, CaseError, LoadedCase};
use do_anything::core::events::EventBus;
use do_anything::core::filter::Filters;
use do_anything::core::output::{self, Format};
use do_anything::core::registry::Registry;
use do_anything::core::report::RunOutcome;
use do_anything::core::runner;
use do_anything::core::schema;
use do_anything::core::secrets::Secrets;
use do_anything::drivers;
use do_anything::exit::ExitCode as DaExitCode;
use do_anything::report as html_report;
use do_anything::session::export::ExportOptions;
use do_anything::session::{
    self, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_SESSIONS_DIR, SessionRequest, StepInput,
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
        /// Runs up to N cases at the same time. Defaults to 1.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_jobs)]
        jobs: usize,
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
    Start,
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
            jobs,
        } => {
            let sources = match VarSources::load(&vars_files, &vars) {
                Ok(sources) => sources,
                Err(message) => return usage_error(&message),
            };
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
                    jobs,
                },
            )
        }
        Command::Report {
            directory,
            output_file,
            shareable,
        } => report(default_output, &directory, output_file, shareable),
        Command::Lint {
            paths,
            vars,
            vars_files,
        } => {
            let sources = match VarSources::load(&vars_files, &vars) {
                Ok(sources) => sources,
                Err(message) => return usage_error(&message),
            };
            lint(&paths, default_output, &registry, &sources)
        }
        Command::Schema => print_schema(&registry, default_output),
        #[cfg(feature = "mcp")]
        Command::Mcp {
            sessions_dir,
            idle_timeout_ms,
        } => do_anything::mcp::serve(
            &registry,
            do_anything::mcp::Options {
                sessions_dir: sessions_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_SESSIONS_DIR)),
                idle_timeout_ms: idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS),
            },
        )
        .into(),
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
        SessionCommand::Start => SessionRequest::Start {
            sessions_dir,
            idle_timeout_ms: idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS),
        },
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
                    Err(message) => {
                        if format == OutputFormat::Json {
                            let err = serde_json::json!({
                                "ok": false,
                                "error": { "kind": "usage", "message": message }
                            });
                            let _ = write_stdout(&format!("{err}\n"));
                        } else {
                            let _ = write_stderr(&format!("error: {message}\n"));
                        }
                        return DaExitCode::Usage.into();
                    }
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
    } else if format == OutputFormat::Json {
        let _ = write_stdout(&format!("{}\n", response.payload));
    } else {
        let rendered = serde_json::to_string_pretty(&response.payload)
            .unwrap_or_else(|_| response.payload.to_string());
        let _ = write_stdout(&format!("{rendered}\n"));
    }
    response.exit_code.into()
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
    jobs: usize,
}

fn run(format: OutputFormat, registry: &Registry, options: RunOptions) -> ExitCode {
    let mut secrets = Secrets::new();
    let outcome = execute(registry, &options, &mut secrets);
    let rendered = output::render(&outcome, format.into(), &secrets);
    // A write error must not replace the run's exit code. The common case is a
    // reader that exits early, such as `head` or `jq`; there is usually nowhere
    // left to report the failure either.
    let _ = write_stdout(&rendered.stdout);
    let _ = write_stderr(&rendered.stderr);
    outcome.exit_code().into()
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
    runner::run_with_record(
        &cases,
        registry,
        &bus,
        bundles.as_mut(),
        record.as_mut(),
        &options.filters,
        secrets,
        options.jobs,
    )
}

/// Validates the cases without running them.
///
/// It shares discovery, loading and [`runner::preflight`] with `da run`, so the
/// two commands cannot disagree about which cases are valid. Unlike a run, it
/// keeps going after the first bad file and reports every problem at once.
fn lint(
    paths: &[PathBuf],
    format: OutputFormat,
    registry: &Registry,
    sources: &VarSources,
) -> ExitCode {
    let mut secrets = Secrets::new();
    let LoadedCases {
        files,
        cases,
        errors,
        ..
    } = load_cases(paths, sources, &mut secrets);
    let mut errors = errors;
    errors.extend(runner::preflight(&cases, registry, &mut secrets));
    let warnings = runner::preflight_warnings(&cases, registry, &mut secrets);

    let report = output::LintReport {
        files: files.len(),
        errors,
        warnings,
        cases: cases.iter().map(output::LintCase::new).collect(),
    };
    let rendered = output::render_lint(&report, format.into(), &secrets);
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
    for (file, entries) in &sources.files {
        for (name, value) in entries {
            match case::expand_env(value, secrets) {
                Ok(value) => overrides.push((name.clone(), value)),
                Err(message) => errors.push(CaseError {
                    file: file.clone(),
                    message: format!("variable `{name}`: {message}"),
                }),
            }
        }
    }
    overrides.extend(sources.inline.iter().cloned());

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
