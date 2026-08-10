use anyhow::Context;
use clap::Args;
use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_exec::Cli as ExecCli;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::SharedCliOptions;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Run and validate Workflow scripts.
#[derive(Debug, Args)]
#[command(
    after_help = "Examples:\n  codex++ workflow run my-workflow --args '{\"topic\":\"rust\"}'\n  codex++ workflow run ./.claude/workflows/review.js\n  codex++ workflow run review-changes --resume wf_abc123\n  codex++ workflow validate ./.claude/workflows/review.js --json"
)]
pub(crate) struct WorkflowCli {
    #[command(subcommand)]
    pub(crate) command: WorkflowSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum WorkflowSubcommand {
    /// Launch a workflow through one ordinary Codex exec turn and wait for it.
    #[command(
        after_help = "The run command is a convenience wrapper around `codex++ exec`; the model\nlaunches the existing Workflow tool and waits for the terminal result with\nWaitWorkflow."
    )]
    Run(WorkflowRunArgs),

    /// Validate a workflow script without starting a model turn.
    Validate(WorkflowValidateArgs),
}

#[derive(Debug, Args)]
pub(crate) struct WorkflowRunArgs {
    /// Workflow name, or a path to an existing .js workflow script.
    ///
    /// A value ending in .js, containing a path separator, or starting with
    /// ./, ../, /, or ~/ is passed as scriptPath; every other value is a name.
    #[arg(value_name = "NAME_OR_SCRIPT")]
    pub(crate) source: String,

    /// JSON value passed to the workflow as its `args` global.
    ///
    /// When omitted, the Workflow tool receives no `args` field and the script
    /// observes `undefined`; pass `null` explicitly for a literal null.
    #[arg(long, value_name = "JSON")]
    pub(crate) args: Option<String>,

    /// Resume an existing run by id.
    ///
    /// When omitted, start a new run. When set, omit `--args` to reuse the
    /// persisted arguments and their presence.
    #[arg(long, value_name = "RUN_ID")]
    pub(crate) resume: Option<String>,

    /// Forward JSONL events from the underlying `codex++ exec` turn.
    #[arg(long, default_value_t = false)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WorkflowValidateArgs {
    /// Path to the workflow .js script to validate.
    #[arg(value_name = "PATH")]
    pub(crate) path: PathBuf,

    /// Print a machine-readable JSON result to stdout.
    #[arg(long, default_value_t = false)]
    pub(crate) json: bool,
}

pub(crate) async fn run(
    cli: WorkflowCli,
    root_shared: &SharedCliOptions,
    root_strict_config: bool,
    root_config_overrides: CliConfigOverrides,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    match cli.command {
        WorkflowSubcommand::Run(args) => {
            run_workflow(
                args,
                root_shared,
                root_strict_config,
                root_config_overrides,
                arg0_paths,
            )
            .await
        }
        WorkflowSubcommand::Validate(args) => validate_workflow(args),
    }
}

async fn run_workflow(
    args: WorkflowRunArgs,
    root_shared: &SharedCliOptions,
    root_strict_config: bool,
    root_config_overrides: CliConfigOverrides,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    let workflow_args = parse_workflow_args(args.args.as_deref())?;
    let source = classify_workflow_source(&args.source);
    let prompt = workflow_prompt(&source, workflow_args.as_ref(), args.resume.as_deref());

    let mut exec_cli = ExecCli::try_parse_from(["codex", "exec"])?;
    exec_cli.shared.inherit_exec_root_options(root_shared);
    exec_cli.strict_config |= root_strict_config;
    exec_cli
        .config_overrides
        .prepend_root_overrides(root_config_overrides);
    // The command is only useful when the Workflow extension is registered. Keep
    // this last so it wins over a root `--disable workflows` or config value.
    exec_cli
        .config_overrides
        .raw_overrides
        .push("features.workflows=true".to_string());
    exec_cli.prompt = Some(prompt);
    exec_cli.json = args.json;

    let codex_home = codex_core::config::find_codex_home()?.to_path_buf();
    let started_at = unix_seconds();
    Box::pin(codex_exec::run_main(exec_cli, arg0_paths)).await?;
    verify_workflow_outcome(&codex_home, started_at)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn latest_workflow_snapshot(codex_home: &Path, since: i64) -> Option<(PathBuf, Value)> {
    let mut best: Option<(i64, PathBuf, Value)> = None;
    let threads = std::fs::read_dir(codex_home.join("sessions")).ok()?;
    for thread in threads.flatten() {
        let workflows = thread.path().join("workflows");
        let Ok(entries) = std::fs::read_dir(workflows) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_str::<Value>(&source) else {
                continue;
            };
            let started_at = snapshot
                .get("startedAt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if started_at < since {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(best_started_at, _, _)| started_at > *best_started_at)
            {
                best = Some((started_at, path, snapshot));
            }
        }
    }
    best.map(|(_, path, snapshot)| (path, snapshot))
}

fn verify_workflow_outcome(codex_home: &Path, since: i64) -> anyhow::Result<()> {
    let Some((path, snapshot)) = latest_workflow_snapshot(codex_home, since) else {
        anyhow::bail!(
            "workflow run did not launch a workflow; no workflow snapshot was written after the command started"
        );
    };
    let status = snapshot
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match status {
        "completed" => Ok(()),
        "failed" | "killed" | "paused" | "pending" | "running" => {
            let run_id = snapshot
                .get("runId")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let error = snapshot
                .get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            anyhow::bail!(
                "workflow {run_id} ended with status {status}{error} (snapshot: {})",
                path.display()
            )
        }
        other => anyhow::bail!(
            "workflow snapshot {} has unknown status {other}",
            path.display()
        ),
    }
}

fn validate_workflow(args: WorkflowValidateArgs) -> anyhow::Result<()> {
    let result = validate_workflow_file(&args.path);
    match result {
        Ok(meta) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "valid": true,
                        "path": args.path.display().to_string(),
                        "meta": meta,
                    }))?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            }
            Ok(())
        }
        Err(error) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "valid": false,
                        "path": args.path.display().to_string(),
                        "error": error,
                    }))?
                );
            }
            anyhow::bail!("invalid workflow script {}: {error}", args.path.display())
        }
    }
}

fn validate_workflow_file(path: &Path) -> Result<Value, String> {
    if path.extension().and_then(std::ffi::OsStr::to_str) != Some("js") {
        return Err(format!(
            "workflow scripts must use the .js extension: {}",
            path.display()
        ));
    }
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect workflow script {}", path.display()))
        .map_err(|error| format!("{error:#}"))?;
    if metadata.len() > codex_workflow::MAX_WORKFLOW_SCRIPT_BYTES as u64 {
        return Err(format!(
            "workflow script is {} bytes; the maximum is {} bytes",
            metadata.len(),
            codex_workflow::MAX_WORKFLOW_SCRIPT_BYTES
        ));
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow script {}", path.display()))
        .map_err(|error| format!("{error:#}"))?;
    let validated =
        codex_workflow::validate_workflow_script(source).map_err(|error| error.to_string())?;
    serde_json::to_value(validated.meta).map_err(|error| error.to_string())
}

fn parse_workflow_args(raw: Option<&str>) -> anyhow::Result<Option<Value>> {
    match raw {
        None => Ok(None),
        Some(raw) => serde_json::from_str(raw)
            .map(Some)
            .with_context(|| format!("`--args` must be valid JSON; received {raw:?}")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkflowSource {
    Name(String),
    ScriptPath(PathBuf),
}

fn classify_workflow_source(value: &str) -> WorkflowSource {
    let looks_like_path = value.ends_with(".js")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('/')
        || value.starts_with("~/")
        || value.contains('/')
        || value.contains('\\');
    if looks_like_path {
        WorkflowSource::ScriptPath(PathBuf::from(value))
    } else {
        WorkflowSource::Name(value.to_string())
    }
}

fn workflow_prompt(
    source: &WorkflowSource,
    args: Option<&Value>,
    resume_from_run_id: Option<&str>,
) -> String {
    let source_fields = match source {
        WorkflowSource::Name(name) => serde_json::json!({ "name": name }),
        WorkflowSource::ScriptPath(path) => {
            serde_json::json!({ "scriptPath": path.display().to_string() })
        }
    };
    let mut call = source_fields.as_object().cloned().unwrap_or_default();
    if let Some(args) = args {
        call.insert("args".to_string(), args.clone());
    }
    if let Some(run_id) = resume_from_run_id {
        call.insert(
            "resumeFromRunId".to_string(),
            Value::String(run_id.to_string()),
        );
    }
    let call =
        serde_json::to_string_pretty(&Value::Object(call)).unwrap_or_else(|_| "{}".to_string());
    let args_instruction = match args {
        Some(args) => {
            let args = serde_json::to_string_pretty(args).unwrap_or_else(|_| "null".to_string());
            format!(
                "The `args` value above is the JSON value for the Workflow tool's `args` field:\n\n\
{args}"
            )
        }
        None if resume_from_run_id.is_some() => {
            "The call above intentionally omits the `args` field. Do not add it and do not\n\
send `args: null`; the Workflow tool must reuse the resumed run's persisted arguments and\n\
their presence."
                .to_string()
        }
        None => "The call above intentionally omits the `args` field. Do not add it and do not\n\
send `args: null`; the script must observe `undefined`."
            .to_string(),
    };

    format!(
        "Run the requested Codex Workflow through its terminal state.\n\n\
Call the `Workflow` tool exactly once with this exact source and arguments:\n\n\
{call}\n\n\
{args_instruction}\n\n\
Requirements:\n\
1. Use only the source shown above; do not use an inline `script` and do not add or replace source fields.\n\
2. The Workflow tool launches in the background and returns a `runId`. Then call `WaitWorkflow` with that `runId` and wait for a terminal state. If it returns a timeout while the run is still active, call `WaitWorkflow` again with the same run id until the run is terminal.\n\
3. Do not finish the turn while the workflow is still running, and do not use status tools as a substitute for waiting.\n\
4. When the workflow is terminal, return the final result from `WaitWorkflow` verbatim. If it failed or was cancelled, include the terminal status and error details.\n\
5. Do not create or edit workflow scripts.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TestSubcommand,
    }

    #[derive(Debug, clap::Subcommand)]
    enum TestSubcommand {
        #[command(
            after_help = "Examples:\n  codex++ workflow run my-workflow --args '{\"topic\":\"rust\"}'\n  codex++ workflow run ./.claude/workflows/review.js\n  codex++ workflow run review-changes --resume wf_abc123\n  codex++ workflow validate ./.claude/workflows/review.js --json"
        )]
        Workflow(WorkflowCli),
    }

    #[test]
    fn classifies_names_paths_and_js_values() {
        assert_eq!(
            classify_workflow_source("missing-workflow-name-xyz"),
            WorkflowSource::Name("missing-workflow-name-xyz".to_string())
        );
        assert_eq!(
            classify_workflow_source("missing-script.js"),
            WorkflowSource::ScriptPath(PathBuf::from("missing-script.js"))
        );
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let existing = temp_dir.path().join("workflow.txt");
        std::fs::write(&existing, "not a script").expect("write existing file");
        assert_eq!(
            classify_workflow_source(existing.to_string_lossy().as_ref()),
            WorkflowSource::ScriptPath(existing)
        );
        // A bare workflow name must not become a script path just because a
        // file with the same name exists in the current directory.
        assert_eq!(
            classify_workflow_source("README.md"),
            WorkflowSource::Name("README.md".to_string())
        );
    }

    #[test]
    fn args_remain_absent_unless_explicitly_provided() {
        assert_eq!(parse_workflow_args(None).expect("absent args"), None);
        assert_eq!(
            parse_workflow_args(Some(r#"{"topic":"rust"}"#)).expect("parsed args"),
            Some(serde_json::json!({ "topic": "rust" }))
        );
        assert_eq!(
            parse_workflow_args(Some("null")).expect("explicit null"),
            Some(Value::Null)
        );
        assert!(parse_workflow_args(Some("not-json")).is_err());
    }

    #[test]
    fn prompt_uses_name_and_requires_wait_by_default() {
        let args = serde_json::json!({ "topic": "rust" });
        let prompt = workflow_prompt(
            &WorkflowSource::Name("my-workflow".to_string()),
            Some(&args),
            None,
        );
        assert!(prompt.contains(r#""name": "my-workflow""#), "{prompt}");
        assert!(!prompt.contains("scriptPath"), "{prompt}");
        assert!(prompt.contains("WaitWorkflow"), "{prompt}");
        assert!(prompt.contains(r#""topic": "rust""#), "{prompt}");
        assert!(!prompt.contains("intentionally omits"), "{prompt}");
    }

    #[test]
    fn prompt_omits_args_when_the_flag_is_absent() {
        let prompt = workflow_prompt(
            &WorkflowSource::ScriptPath(PathBuf::from("./review.js")),
            None,
            None,
        );
        assert!(
            prompt.contains(r#""scriptPath": "./review.js""#),
            "{prompt}"
        );
        assert!(!prompt.contains(r#""args""#), "{prompt}");
        assert!(prompt.contains("observe `undefined`"), "{prompt}");
        assert!(prompt.contains("WaitWorkflow"), "{prompt}");
    }

    #[test]
    fn prompt_carries_resume_id_and_keeps_args_absent() {
        let prompt = workflow_prompt(
            &WorkflowSource::Name("my-workflow".to_string()),
            None,
            Some("wf_123"),
        );
        assert!(
            prompt.contains(r#""resumeFromRunId": "wf_123""#),
            "{prompt}"
        );
        assert!(!prompt.contains(r#""args""#), "{prompt}");
        assert!(
            prompt.contains("reuse the resumed run's persisted arguments"),
            "{prompt}"
        );
        assert!(!prompt.contains("observe `undefined`"), "{prompt}");
    }

    #[test]
    fn prompt_keeps_an_explicit_null_arg() {
        let prompt = workflow_prompt(
            &WorkflowSource::ScriptPath(PathBuf::from("./review.js")),
            Some(&Value::Null),
            None,
        );
        assert!(prompt.contains(r#""args": null"#), "{prompt}");
        assert!(!prompt.contains("intentionally omits"), "{prompt}");
    }

    #[test]
    fn parses_run_and_validate_arguments() {
        let cli = TestCli::try_parse_from([
            "codex",
            "workflow",
            "run",
            "my-workflow",
            "--args",
            r#"{"x":1}"#,
            "--json",
        ])
        .expect("parse run");
        let TestSubcommand::Workflow(WorkflowCli {
            command: WorkflowSubcommand::Run(resumed),
        }) = TestCli::try_parse_from([
            "codex",
            "workflow",
            "run",
            "my-workflow",
            "--resume",
            "wf_123",
        ])
        .expect("parse resumed run")
        .command
        else {
            panic!("expected run");
        };
        assert_eq!(resumed.resume.as_deref(), Some("wf_123"));
        assert!(resumed.args.is_none());
        let TestSubcommand::Workflow(WorkflowCli {
            command: WorkflowSubcommand::Run(args),
        }) = cli.command
        else {
            panic!("expected run");
        };
        assert_eq!(args.source, "my-workflow");
        assert_eq!(args.args.as_deref(), Some(r#"{"x":1}"#));
        assert!(args.json);

        let cli = TestCli::try_parse_from(["codex", "workflow", "validate", "review.js", "--json"])
            .expect("parse validate");
        let TestSubcommand::Workflow(WorkflowCli {
            command: WorkflowSubcommand::Validate(args),
        }) = cli.command
        else {
            panic!("expected validate");
        };
        assert_eq!(args.path, PathBuf::from("review.js"));
        assert!(args.json);
    }

    #[test]
    fn help_documents_examples() {
        let help = TestCli::command()
            .try_get_matches_from(["codex", "workflow", "--help"])
            .expect_err("help should short-circuit");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = help.to_string();
        assert!(help.contains("codex++ workflow run"), "{help}");
        assert!(help.contains("codex++ workflow validate"), "{help}");
    }

    #[test]
    fn validates_scripts_without_a_model() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let valid = temp_dir.path().join("valid.js");
        std::fs::write(
            &valid,
            "export const meta = { name: 'demo', description: 'demo workflow' };\nreturn args;\n",
        )
        .expect("write valid script");
        let meta = validate_workflow_file(&valid).expect("valid script");
        assert_eq!(meta["name"], "demo");

        let invalid = temp_dir.path().join("invalid.js");
        std::fs::write(&invalid, "return 1;\n").expect("write invalid script");
        let error = validate_workflow_file(&invalid).expect_err("missing meta must fail");
        assert!(error.contains("first statement"), "{error}");

        let wrong_extension = temp_dir.path().join("workflow.txt");
        std::fs::write(&wrong_extension, "return 1;\n").expect("write wrong extension");
        let error = validate_workflow_file(&wrong_extension).expect_err(".js is required");
        assert!(error.contains(".js extension"), "{error}");
    }

    #[test]
    fn verifies_workflow_snapshot_statuses() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let workflows = temp_dir.path().join("sessions/thread-1/workflows");
        std::fs::create_dir_all(&workflows).expect("workflow dir");
        let snapshot = workflows.join("wf_1.json");

        std::fs::write(
            &snapshot,
            serde_json::json!({
                "runId": "wf_1",
                "startedAt": 100,
                "status": "completed",
            })
            .to_string(),
        )
        .expect("write completed snapshot");
        assert!(verify_workflow_outcome(temp_dir.path(), 99).is_ok());

        std::fs::write(
            &snapshot,
            serde_json::json!({
                "runId": "wf_1",
                "startedAt": 100,
                "status": "failed",
                "error": "boom",
            })
            .to_string(),
        )
        .expect("write failed snapshot");
        let error = verify_workflow_outcome(temp_dir.path(), 99).expect_err("failed workflow");
        assert!(error.to_string().contains("status failed"), "{error}");
        assert!(error.to_string().contains("boom"), "{error}");

        let empty = tempfile::tempdir().expect("empty temp dir");
        let error = verify_workflow_outcome(empty.path(), 99).expect_err("missing snapshot");
        assert!(error.to_string().contains("did not launch"), "{error}");
    }
}
