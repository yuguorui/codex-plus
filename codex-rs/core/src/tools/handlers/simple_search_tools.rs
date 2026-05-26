use codex_exec_server::FileSystemSandboxContext;
use codex_install_context::InstallContext;
use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_utils_absolute_path::AbsolutePathBuf;
use regex_lite::Regex;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;
use tokio::time::timeout;

use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::simple_search_tools_spec::GLOB_TOOL_NAME;
use crate::tools::handlers::simple_search_tools_spec::GREP_TOOL_NAME;
use crate::tools::handlers::simple_search_tools_spec::SimpleSearchToolOptions;
use crate::tools::handlers::simple_search_tools_spec::create_glob_tool;
use crate::tools::handlers::simple_search_tools_spec::create_grep_tool;
use crate::tools::handlers::simple_tool_output::GlobOutput;
use crate::tools::handlers::simple_tool_output::GrepOutput;
use crate::tools::handlers::simple_tool_output::GrepOutputModeSchema;
use crate::tools::handlers::simple_tool_output::TextStructuredOutput;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

const MAX_GLOB_RESULTS: usize = 100;
const DEFAULT_GREP_HEAD_LIMIT: usize = 250;
const RIPGREP_TIMEOUT: Duration = Duration::from_secs(20);
const VCS_DIRECTORIES_TO_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

pub(crate) struct GlobHandler {
    options: SimpleSearchToolOptions,
}

impl GlobHandler {
    pub(crate) fn new(options: SimpleSearchToolOptions) -> Self {
        Self { options }
    }
}

pub(crate) struct GrepHandler {
    options: SimpleSearchToolOptions,
}

impl GrepHandler {
    pub(crate) fn new(options: SimpleSearchToolOptions) -> Self {
        Self { options }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    #[serde(default)]
    output_mode: GrepOutputMode,
    #[serde(default, deserialize_with = "deserialize_bool")]
    multiline: bool,
    #[serde(
        default,
        rename = "-B",
        deserialize_with = "deserialize_optional_usize"
    )]
    context_before: Option<usize>,
    #[serde(
        default,
        rename = "-A",
        deserialize_with = "deserialize_optional_usize"
    )]
    context_after: Option<usize>,
    #[serde(
        default,
        rename = "-C",
        deserialize_with = "deserialize_optional_usize"
    )]
    context: Option<usize>,
    #[serde(
        default,
        rename = "context",
        deserialize_with = "deserialize_optional_usize"
    )]
    context_long: Option<usize>,
    #[serde(default, rename = "-n", deserialize_with = "deserialize_optional_bool")]
    show_line_numbers: Option<bool>,
    #[serde(default, rename = "-i", deserialize_with = "deserialize_bool")]
    case_insensitive: bool,
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    head_limit: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_usize")]
    offset: usize,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchFile {
    path: AbsolutePathBuf,
    display_path: String,
    modified_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepFileMatch {
    display_path: String,
    line_matches: Vec<GrepLineMatch>,
    count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepLineMatch {
    line_number: usize,
    text: String,
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for GlobHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(GLOB_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_glob_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "Glob handler received unsupported payload".to_string(),
            ));
        };
        let args: GlobArgs = parse_arguments(&arguments)?;
        emit_simple_tool_started(session.as_ref(), turn.as_ref(), &call_id, "Glob", &args).await;
        let started_at = Instant::now();
        let output_result = async {
            let Some(turn_environment) =
                resolve_tool_environment(turn.as_ref(), args.environment_id.as_deref())?
            else {
                return Err(FunctionCallError::RespondToModel(
                    "Glob is unavailable in this session".to_string(),
                ));
            };
            let cwd = turn_environment.cwd.clone();
            let root = resolve_search_path(args.path.as_deref(), &cwd);
            let sandbox =
                turn.file_system_sandbox_context(/*additional_permissions*/ None, &cwd);
            if can_use_local_ripgrep(turn_environment, &sandbox)
                && let Some(paths) = glob_with_ripgrep(&args.pattern, &root).await?
            {
                return Ok(paths
                    .into_iter()
                    .map(|path| display_path(&path, &cwd))
                    .collect::<Vec<_>>());
            }

            let mut files =
                collect_glob_files(turn_environment, &root, &cwd, Some(&sandbox), &args.pattern)
                    .await?;
            files.sort_by(|left, right| {
                right
                    .modified_at_ms
                    .cmp(&left.modified_at_ms)
                    .then_with(|| left.display_path.cmp(&right.display_path))
            });

            Ok(files
                .into_iter()
                .map(|file| file.display_path)
                .collect::<Vec<_>>())
        }
        .await;
        let paths = match output_result {
            Ok(paths) => paths,
            Err(err) => {
                emit_simple_tool_failed(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    "Glob",
                    &args,
                    &err.to_string(),
                )
                .await;
                return Err(err);
            }
        };
        emit_simple_tool_completed(session.as_ref(), turn.as_ref(), &call_id, "Glob", &args).await;
        let output = glob_output(paths, started_at.elapsed());
        let text = render_glob_output(&output);
        Ok(boxed_tool_output(TextStructuredOutput::new(text, output)))
    }
}

impl CoreToolRuntime for GlobHandler {}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for GrepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(GREP_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_grep_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "Grep handler received unsupported payload".to_string(),
            ));
        };
        let args: GrepArgs = parse_arguments(&arguments)?;
        emit_simple_tool_started(session.as_ref(), turn.as_ref(), &call_id, "Grep", &args).await;
        let output_result = async {
            let Some(turn_environment) =
                resolve_tool_environment(turn.as_ref(), args.environment_id.as_deref())?
            else {
                return Err(FunctionCallError::RespondToModel(
                    "Grep is unavailable in this session".to_string(),
                ));
            };
            let cwd = turn_environment.cwd.clone();
            let root = resolve_search_path(args.path.as_deref(), &cwd);
            let sandbox =
                turn.file_system_sandbox_context(/*additional_permissions*/ None, &cwd);
            if can_use_local_ripgrep(turn_environment, &sandbox)
                && let Some(output) = grep_with_ripgrep(&args, &root, &cwd).await?
            {
                return Ok(output);
            }

            let files =
                collect_grep_files(turn_environment, &root, &cwd, Some(&sandbox), &args).await?;
            let regex = compile_grep_regex(&args)?;
            let matches =
                search_files(turn_environment, files, Some(&sandbox), &regex, &args).await?;

            Ok(render_grep_matches_with_args(&matches, &args))
        }
        .await;
        let output = match output_result {
            Ok(output) => output,
            Err(err) => {
                emit_simple_tool_failed(
                    session.as_ref(),
                    turn.as_ref(),
                    &call_id,
                    "Grep",
                    &args,
                    &err.to_string(),
                )
                .await;
                return Err(err);
            }
        };
        emit_simple_tool_completed(session.as_ref(), turn.as_ref(), &call_id, "Grep", &args).await;
        Ok(boxed_tool_output(TextStructuredOutput::new(
            output.text,
            output.structured,
        )))
    }
}

impl CoreToolRuntime for GrepHandler {}

async fn collect_grep_files(
    turn_environment: &TurnEnvironment,
    root: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    args: &GrepArgs,
) -> Result<Vec<SearchFile>, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    let metadata = fs.get_metadata(root, sandbox).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "unable to access search path `{}`: {error}",
            root.display()
        ))
    })?;
    let mut files = if metadata.is_file {
        vec![SearchFile {
            path: root.clone(),
            display_path: display_path(root, cwd),
            modified_at_ms: metadata.modified_at_ms,
        }]
    } else if metadata.is_directory {
        collect_files(turn_environment, root, cwd, sandbox).await?
    } else {
        Vec::new()
    };

    if let Some(glob) = &args.glob {
        let matcher = GlobMatcher::new(glob)?;
        files.retain(|file| matcher.is_match(&relative_to_root(&file.path, root)));
    }
    if let Some(file_type) = &args.file_type {
        let extensions = type_extensions(file_type);
        files.retain(|file| {
            file.path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extensions.iter().any(|candidate| candidate == &extension))
        });
    }

    files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    Ok(files)
}

async fn collect_glob_files(
    turn_environment: &TurnEnvironment,
    root: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
    pattern: &str,
) -> Result<Vec<SearchFile>, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    let metadata = fs.get_metadata(root, sandbox).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "unable to access search path `{}`: {error}",
            root.display()
        ))
    })?;
    if metadata.is_file {
        return Ok(vec![SearchFile {
            path: root.clone(),
            display_path: display_path(root, cwd),
            modified_at_ms: metadata.modified_at_ms,
        }]);
    }
    if !metadata.is_directory {
        return Ok(Vec::new());
    }

    let matcher = GlobMatcher::new(pattern)?;
    let mut files = Vec::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        let entries = fs.read_directory(&dir, sandbox).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "unable to read directory `{}`: {error}",
                dir.display()
            ))
        })?;
        for entry in entries {
            let path = dir.join(entry.file_name);
            if entry.is_directory {
                dirs.push(path);
            } else if entry.is_file && matcher.is_match(&relative_to_root(&path, root)) {
                let Ok(metadata) = fs.get_metadata(&path, sandbox).await else {
                    continue;
                };
                files.push(SearchFile {
                    display_path: display_path(&path, cwd),
                    path,
                    modified_at_ms: metadata.modified_at_ms,
                });
            }
        }
    }
    Ok(files)
}

async fn collect_files(
    turn_environment: &TurnEnvironment,
    root: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<Vec<SearchFile>, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    let metadata = fs.get_metadata(root, sandbox).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "unable to access search path `{}`: {error}",
            root.display()
        ))
    })?;
    if metadata.is_file {
        return Ok(vec![SearchFile {
            path: root.clone(),
            display_path: display_path(root, cwd),
            modified_at_ms: metadata.modified_at_ms,
        }]);
    }
    if !metadata.is_directory {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        let entries = fs.read_directory(&dir, sandbox).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "unable to read directory `{}`: {error}",
                dir.display()
            ))
        })?;
        for entry in entries {
            let path = dir.join(entry.file_name);
            if entry.is_directory {
                dirs.push(path);
            } else if entry.is_file {
                let Ok(metadata) = fs.get_metadata(&path, sandbox).await else {
                    continue;
                };
                files.push(SearchFile {
                    display_path: display_path(&path, cwd),
                    path,
                    modified_at_ms: metadata.modified_at_ms,
                });
            }
        }
    }
    Ok(files)
}

fn can_use_local_ripgrep(
    turn_environment: &TurnEnvironment,
    sandbox: &FileSystemSandboxContext,
) -> bool {
    !turn_environment.environment.is_remote() && !sandbox.should_run_in_sandbox()
}

async fn glob_with_ripgrep(
    pattern: &str,
    root: &AbsolutePathBuf,
) -> Result<Option<Vec<AbsolutePathBuf>>, FunctionCallError> {
    let output = run_ripgrep(
        [
            "--files",
            "--hidden",
            "--no-ignore",
            "--sort=modified",
            "--null",
            "--glob",
            pattern,
            "--",
        ],
        root,
    )
    .await?;
    let Some(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.is_empty() {
            return Ok(Some(Vec::new()));
        }
        return Err(ripgrep_error("Glob", root, &output));
    }

    output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
            AbsolutePathBuf::from_absolute_path_checked(if path.is_absolute() {
                path
            } else {
                root.join(path).into_path_buf()
            })
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

async fn grep_with_ripgrep(
    args: &GrepArgs,
    root: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
) -> Result<Option<GrepRenderedOutput>, FunctionCallError> {
    let mut rg_args = vec!["--hidden".to_string()];
    for dir in VCS_DIRECTORIES_TO_EXCLUDE {
        rg_args.push("--glob".to_string());
        rg_args.push(format!("!{dir}"));
    }
    rg_args.push("--max-columns".to_string());
    rg_args.push("500".to_string());

    if args.multiline {
        rg_args.push("-U".to_string());
        rg_args.push("--multiline-dotall".to_string());
    }
    if args.case_insensitive {
        rg_args.push("-i".to_string());
    }
    match args.output_mode {
        GrepOutputMode::FilesWithMatches => rg_args.push("-l".to_string()),
        GrepOutputMode::Content => {
            if args.show_line_numbers() {
                rg_args.push("-n".to_string());
            }
        }
        GrepOutputMode::Count => rg_args.push("-c".to_string()),
    }
    if args.output_mode == GrepOutputMode::Content {
        if let Some(context) = args.context_long.or(args.context) {
            rg_args.push("-C".to_string());
            rg_args.push(context.to_string());
        } else {
            if let Some(context_before) = args.context_before {
                rg_args.push("-B".to_string());
                rg_args.push(context_before.to_string());
            }
            if let Some(context_after) = args.context_after {
                rg_args.push("-A".to_string());
                rg_args.push(context_after.to_string());
            }
        }
    }
    if args.pattern.starts_with('-') {
        rg_args.push("-e".to_string());
    }
    rg_args.push(args.pattern.clone());
    if let Some(file_type) = &args.file_type {
        rg_args.push("--type".to_string());
        rg_args.push(file_type.clone());
    }
    if let Some(glob) = &args.glob {
        for glob_pattern in split_glob_patterns(glob) {
            rg_args.push("--glob".to_string());
            rg_args.push(glob_pattern);
        }
    }
    rg_args.push("--".to_string());

    let output = run_ripgrep(rg_args.iter().map(String::as_str), root).await?;
    let Some(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.is_empty() {
            return Ok(Some(render_grep_lines_with_args(Vec::new(), args)));
        }
        return Err(ripgrep_error("Grep", root, &output));
    }

    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| relativize_ripgrep_line(line, root, cwd, args.output_mode))
        .collect::<Vec<_>>();
    Ok(Some(render_grep_lines_with_args(lines, args)))
}

async fn run_ripgrep<'a>(
    args: impl IntoIterator<Item = &'a str>,
    root: &AbsolutePathBuf,
) -> Result<Option<std::process::Output>, FunctionCallError> {
    let mut command = Command::new(InstallContext::current().rg_command());
    command.args(args).arg(root.as_path());
    let output = match timeout(RIPGREP_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok(Err(error)) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "ripgrep failed under `{}`: {error}",
                root.display()
            )));
        }
        Err(_) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "ripgrep timed out after {} seconds under `{}`. Try searching a more specific path or pattern.",
                RIPGREP_TIMEOUT.as_secs(),
                root.display()
            )));
        }
    };
    Ok(Some(output))
}

fn ripgrep_error(
    tool_name: &str,
    root: &AbsolutePathBuf,
    output: &std::process::Output,
) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "{tool_name} ripgrep search failed under `{}` with status {}: {}",
        root.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn split_glob_patterns(glob: &str) -> Vec<String> {
    glob.split_whitespace()
        .flat_map(|raw_pattern| {
            if raw_pattern.contains('{') && raw_pattern.contains('}') {
                vec![raw_pattern.to_string()]
            } else {
                raw_pattern
                    .split(',')
                    .filter(|pattern| !pattern.is_empty())
                    .map(ToString::to_string)
                    .collect()
            }
        })
        .collect()
}

fn relativize_ripgrep_line(
    line: &str,
    root: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    output_mode: GrepOutputMode,
) -> String {
    match output_mode {
        GrepOutputMode::FilesWithMatches => display_ripgrep_path(line, root, cwd),
        GrepOutputMode::Count => {
            if root.as_path().is_file() {
                return line.to_string();
            }
            let Some((path, count)) = line.rsplit_once(':') else {
                return line.to_string();
            };
            format!("{}:{count}", display_ripgrep_path(path, root, cwd))
        }
        GrepOutputMode::Content => {
            if root.as_path().is_file() {
                return line.to_string();
            }
            let Some((path, rest)) = line.split_once(':') else {
                return line.to_string();
            };
            format!("{}:{rest}", display_ripgrep_path(path, root, cwd))
        }
    }
}

fn display_ripgrep_path(path: &str, root: &AbsolutePathBuf, cwd: &AbsolutePathBuf) -> String {
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path.clone()
    } else {
        root.join(&path).into_path_buf()
    };
    AbsolutePathBuf::from_absolute_path(absolute)
        .map(|path| display_path(&path, cwd))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

async fn search_files(
    turn_environment: &TurnEnvironment,
    files: Vec<SearchFile>,
    sandbox: Option<&FileSystemSandboxContext>,
    regex: &Regex,
    args: &GrepArgs,
) -> Result<Vec<GrepFileMatch>, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    let mut matches = Vec::new();
    let mut result_count = 0;
    for file in files {
        if result_count >= args.collection_cap() {
            break;
        }
        if is_excluded_vcs_path(&file.path) {
            continue;
        }
        let Ok(contents) = fs.read_file_text(&file.path, sandbox).await else {
            continue;
        };
        let Some(file_match) = grep_file(&file.display_path, &contents, regex, args) else {
            continue;
        };
        result_count += match args.output_mode {
            GrepOutputMode::FilesWithMatches | GrepOutputMode::Count => 1,
            GrepOutputMode::Content => file_match.line_matches.len().max(1),
        };
        matches.push(file_match);
    }
    Ok(matches)
}

fn grep_file(
    display_path: &str,
    contents: &str,
    regex: &Regex,
    args: &GrepArgs,
) -> Option<GrepFileMatch> {
    if args.multiline {
        let line_starts = line_starts(contents);
        let line_matches = regex
            .find_iter(contents)
            .take(args.collection_cap())
            .map(|matched| GrepLineMatch {
                line_number: line_number_for_offset(&line_starts, matched.start()),
                text: compact_match_text(matched.as_str()),
            })
            .collect::<Vec<_>>();
        return (!line_matches.is_empty()).then(|| GrepFileMatch {
            display_path: display_path.to_string(),
            count: line_matches.len(),
            line_matches,
        });
    }

    let lines = contents.lines().collect::<Vec<_>>();
    let mut line_matches = Vec::new();
    let mut count = 0;
    for (line_index, line) in lines.iter().enumerate() {
        let match_count = regex.find_iter(line).count();
        if match_count == 0 {
            continue;
        }
        count += match_count;
        if line_matches.len() < args.collection_cap() {
            let before = args.effective_context_before();
            let after = args.effective_context_after();
            let start = line_index.saturating_sub(before);
            let end = (line_index + after + 1).min(lines.len());
            for (context_line_index, context_line) in lines[start..end].iter().enumerate() {
                let line_number = start + context_line_index + 1;
                if line_matches
                    .last()
                    .map(|previous: &GrepLineMatch| previous.line_number == line_number)
                    .unwrap_or(false)
                {
                    continue;
                }
                line_matches.push(GrepLineMatch {
                    line_number,
                    text: (*context_line).to_string(),
                });
                if line_matches.len() >= args.collection_cap() {
                    break;
                }
            }
        }
    }

    (count > 0).then(|| GrepFileMatch {
        display_path: display_path.to_string(),
        line_matches,
        count,
    })
}

fn glob_output(paths: Vec<String>, elapsed: Duration) -> GlobOutput {
    let total = paths.len();
    GlobOutput {
        duration_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
        num_files: total,
        filenames: paths.into_iter().take(MAX_GLOB_RESULTS).collect(),
        truncated: total > MAX_GLOB_RESULTS,
    }
}

fn render_glob_output(output: &GlobOutput) -> String {
    if output.filenames.is_empty() {
        return "No files found".to_string();
    }

    let mut text = output.filenames.join("\n");
    if output.truncated {
        text.push_str("\n(Results are truncated. Consider using a more specific path or pattern.)");
    }
    text
}

fn compile_grep_regex(args: &GrepArgs) -> Result<Regex, FunctionCallError> {
    let mut pattern = String::new();
    if args.case_insensitive || args.multiline {
        pattern.push_str("(?");
        if args.case_insensitive {
            pattern.push('i');
        }
        if args.multiline {
            pattern.push('s');
        }
        pattern.push(')');
    }
    pattern.push_str(&args.pattern);
    Regex::new(&pattern).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "invalid grep pattern `{}`: {error}",
            args.pattern
        ))
    })
}

impl GrepArgs {
    fn effective_context_before(&self) -> usize {
        self.context_long
            .or(self.context)
            .or(self.context_before)
            .unwrap_or(0)
    }

    fn effective_context_after(&self) -> usize {
        self.context_long
            .or(self.context)
            .or(self.context_after)
            .unwrap_or(0)
    }

    fn head_limit(&self) -> Option<usize> {
        match self.head_limit {
            Some(0) => None,
            Some(limit) => Some(limit),
            None => Some(DEFAULT_GREP_HEAD_LIMIT),
        }
    }

    fn collection_cap(&self) -> usize {
        self.head_limit().map_or(usize::MAX, |limit| {
            limit.saturating_add(self.offset).saturating_add(1)
        })
    }

    fn show_line_numbers(&self) -> bool {
        self.show_line_numbers.unwrap_or(true)
    }
}

struct GrepRenderedOutput {
    text: String,
    structured: GrepOutput,
}

fn render_grep_matches_with_args(matches: &[GrepFileMatch], args: &GrepArgs) -> GrepRenderedOutput {
    if matches.is_empty() {
        let text = match args.output_mode {
            GrepOutputMode::FilesWithMatches => "No files found".to_string(),
            GrepOutputMode::Content | GrepOutputMode::Count => "No matches found".to_string(),
        };
        return grep_rendered_output(text, Vec::new(), args, None, None);
    }

    let lines = match args.output_mode {
        GrepOutputMode::FilesWithMatches => matches
            .iter()
            .map(|matched| matched.display_path.clone())
            .collect::<Vec<_>>(),
        GrepOutputMode::Count => matches
            .iter()
            .map(|matched| format!("{}:{}", matched.display_path, matched.count))
            .collect::<Vec<_>>(),
        GrepOutputMode::Content => matches
            .iter()
            .flat_map(|matched| {
                matched.line_matches.iter().map(|line_match| {
                    render_grep_content_line(matched, line_match, args.show_line_numbers())
                })
            })
            .collect::<Vec<_>>(),
    };
    render_grep_lines_with_args(lines, args)
}

fn render_grep_lines_with_args(lines: Vec<String>, args: &GrepArgs) -> GrepRenderedOutput {
    let limited = apply_head_limit(lines, args);
    let limit_info = format_limit_info(limited.applied_limit, limited.applied_offset);
    let text = match args.output_mode {
        GrepOutputMode::FilesWithMatches => {
            if limited.lines.is_empty() {
                "No files found".to_string()
            } else {
                let mut output = format!(
                    "Found {} {}",
                    limited.lines.len(),
                    plural(limited.lines.len(), "file")
                );
                if !limit_info.is_empty() {
                    output.push(' ');
                    output.push_str(&limit_info);
                }
                output.push('\n');
                output.push_str(&limited.lines.join("\n"));
                output
            }
        }
        GrepOutputMode::Content => {
            let mut output = if limited.lines.is_empty() {
                "No matches found".to_string()
            } else {
                limited.lines.join("\n")
            };
            if !limit_info.is_empty() {
                output.push_str(&format!(
                    "\n\n[Showing results with pagination = {limit_info}]"
                ));
            }
            output
        }
        GrepOutputMode::Count => {
            let raw_content = if limited.lines.is_empty() {
                "No matches found".to_string()
            } else {
                limited.lines.join("\n")
            };
            let (num_files, num_matches) = count_summary(&limited.lines);
            let mut output = format!(
                "{raw_content}\n\nFound {num_matches} total {} across {num_files} {}.",
                plural(num_matches, "occurrence"),
                plural(num_files, "file")
            );
            if !limit_info.is_empty() {
                output.push_str(&format!(" with pagination = {limit_info}"));
            }
            output
        }
    };
    grep_rendered_output(
        text,
        limited.lines,
        args,
        limited.applied_limit,
        limited.applied_offset,
    )
}

fn grep_rendered_output(
    text: String,
    lines: Vec<String>,
    args: &GrepArgs,
    applied_limit: Option<usize>,
    applied_offset: Option<usize>,
) -> GrepRenderedOutput {
    let filenames = grep_filenames_from_lines(&lines, args.output_mode);
    let (content, num_lines, num_matches) = match args.output_mode {
        GrepOutputMode::FilesWithMatches => (None, None, None),
        GrepOutputMode::Content => (Some(text.clone()), Some(lines.len()), None),
        GrepOutputMode::Count => {
            let (_, matches) = count_summary(&lines);
            (None, None, Some(matches))
        }
    };
    let structured = GrepOutput {
        mode: Some(match args.output_mode {
            GrepOutputMode::FilesWithMatches => GrepOutputModeSchema::FilesWithMatches,
            GrepOutputMode::Content => GrepOutputModeSchema::Content,
            GrepOutputMode::Count => GrepOutputModeSchema::Count,
        }),
        num_files: filenames.len(),
        filenames,
        content,
        num_lines,
        num_matches,
        applied_limit,
        applied_offset,
    };
    GrepRenderedOutput { text, structured }
}

fn grep_filenames_from_lines(lines: &[String], mode: GrepOutputMode) -> Vec<String> {
    let mut filenames = Vec::new();
    for line in lines {
        let Some(filename) = grep_filename_from_line(line, mode) else {
            continue;
        };
        if !filenames.iter().any(|existing| existing == filename) {
            filenames.push(filename.to_string());
        }
    }
    filenames
}

fn grep_filename_from_line(line: &str, mode: GrepOutputMode) -> Option<&str> {
    match mode {
        GrepOutputMode::FilesWithMatches => Some(line),
        GrepOutputMode::Count => line.rsplit_once(':').map(|(filename, _)| filename),
        GrepOutputMode::Content => {
            let (filename, _) = line.split_once(':')?;
            Some(filename)
        }
    }
}

struct HeadLimitedLines {
    lines: Vec<String>,
    applied_limit: Option<usize>,
    applied_offset: Option<usize>,
}

fn apply_head_limit(lines: Vec<String>, args: &GrepArgs) -> HeadLimitedLines {
    let offset = args.offset.min(lines.len());
    if args.head_limit == Some(0) {
        return HeadLimitedLines {
            lines: lines[offset..].to_vec(),
            applied_limit: None,
            applied_offset: (args.offset > 0).then_some(args.offset),
        };
    }

    let limit = args.head_limit.unwrap_or(DEFAULT_GREP_HEAD_LIMIT);
    let end = (offset + limit).min(lines.len());
    HeadLimitedLines {
        lines: lines[offset..end].to_vec(),
        applied_limit: (lines.len().saturating_sub(args.offset) > limit).then_some(limit),
        applied_offset: (args.offset > 0).then_some(args.offset),
    }
}

fn format_limit_info(applied_limit: Option<usize>, applied_offset: Option<usize>) -> String {
    let mut parts = Vec::new();
    if let Some(limit) = applied_limit {
        parts.push(format!("limit: {limit}"));
    }
    if let Some(offset) = applied_offset {
        parts.push(format!("offset: {offset}"));
    }
    parts.join(", ")
}

fn count_summary(lines: &[String]) -> (usize, usize) {
    lines.iter().fold((0, 0), |(files, matches), line| {
        let Some((_, count)) = line.rsplit_once(':') else {
            return (files, matches);
        };
        let Ok(count) = count.parse::<usize>() else {
            return (files, matches);
        };
        (files + 1, matches + count)
    })
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn render_grep_content_line(
    matched: &GrepFileMatch,
    line_match: &GrepLineMatch,
    show_line_numbers: bool,
) -> String {
    if show_line_numbers {
        format!(
            "{}:{}:{}",
            matched.display_path, line_match.line_number, line_match.text
        )
    } else {
        format!("{}:{}", matched.display_path, line_match.text)
    }
}

fn resolve_search_path(path: Option<&str>, cwd: &AbsolutePathBuf) -> AbsolutePathBuf {
    path.filter(|path| !path.is_empty()).map_or_else(
        || cwd.clone(),
        |path| AbsolutePathBuf::resolve_path_against_base(PathBuf::from(path), cwd),
    )
}

fn display_path(path: &AbsolutePathBuf, cwd: &AbsolutePathBuf) -> String {
    path.as_path()
        .strip_prefix(cwd.as_path())
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or_else(|| path.display().to_string(), slash_path)
}

fn relative_to_root(path: &AbsolutePathBuf, root: &AbsolutePathBuf) -> String {
    path.as_path()
        .strip_prefix(root.as_path())
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or_else(|| file_name(path.as_path()), slash_path)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|file_name| file_name.to_str())
        .map_or_else(|| slash_path(path), ToString::to_string)
}

fn is_excluded_vcs_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        VCS_DIRECTORIES_TO_EXCLUDE.contains(&name.as_ref())
    })
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn compact_match_text(text: &str) -> String {
    const MAX_MATCH_TEXT_LEN: usize = 200;
    let compact = text.replace('\n', "\\n").replace('\r', "\\r");
    if compact.len() <= MAX_MATCH_TEXT_LEN {
        compact
    } else {
        format!("{}...", &compact[..MAX_MATCH_TEXT_LEN])
    }
}

fn line_starts(contents: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            contents
                .match_indices('\n')
                .map(|(index, newline)| index + newline.len()),
        )
        .collect()
}

fn line_number_for_offset(line_starts: &[usize], offset: usize) -> usize {
    line_starts.partition_point(|start| *start <= offset)
}

fn type_extensions(file_type: &str) -> Vec<&str> {
    match file_type {
        "rust" => vec!["rs"],
        "python" | "py" => vec!["py"],
        "javascript" | "js" => vec!["js", "jsx", "mjs", "cjs"],
        "typescript" | "ts" => vec!["ts", "tsx"],
        "go" => vec!["go"],
        "java" => vec!["java"],
        "c" => vec!["c", "h"],
        "cpp" | "c++" => vec!["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        "markdown" | "md" => vec!["md", "markdown"],
        "json" => vec!["json"],
        "yaml" | "yml" => vec!["yaml", "yml"],
        "toml" => vec!["toml"],
        "text" | "txt" => vec!["txt"],
        extension => vec![extension.trim_start_matches('.')],
    }
}

fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_bool(deserializer).map(|value| value.unwrap_or(false))
}

fn deserialize_optional_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(value) => Ok(Some(value)),
        serde_json::Value::Number(value) if value.as_u64() == Some(0) => Ok(Some(false)),
        serde_json::Value::Number(value) if value.as_u64() == Some(1) => Ok(Some(true)),
        serde_json::Value::String(value) => parse_bool_string(&value).map(Some).ok_or_else(|| {
            de::Error::custom(format!("expected boolean-compatible string, got `{value}`"))
        }),
        _ => Err(de::Error::custom("expected boolean")),
    }
}

fn deserialize_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_usize(deserializer).map(|value| value.unwrap_or(0))
}

fn deserialize_optional_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(value) => value
            .as_u64()
            .ok_or_else(|| de::Error::custom("expected non-negative integer"))
            .and_then(|value| {
                usize::try_from(value).map_err(|_| de::Error::custom("integer is out of range"))
            })
            .map(Some),
        serde_json::Value::String(value) => value
            .parse::<usize>()
            .map(Some)
            .map_err(|_| de::Error::custom(format!("expected integer string, got `{value}`"))),
        _ => Err(de::Error::custom("expected integer")),
    }
}

fn parse_bool_string(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

async fn emit_simple_tool_started<T: serde::Serialize>(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    tool_name: &str,
    args: &T,
) {
    session
        .emit_turn_item_started(
            turn,
            &TurnItem::McpToolCall(McpToolCallItem {
                id: call_id.to_string(),
                server: "codex++".to_string(),
                tool: tool_name.to_string(),
                arguments: serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
                mcp_app_resource_uri: None,
                plugin_id: None,
                status: McpToolCallStatus::InProgress,
                result: None,
                error: None,
                duration: None,
            }),
        )
        .await;
}

async fn emit_simple_tool_completed<T: serde::Serialize>(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    tool_name: &str,
    args: &T,
) {
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::McpToolCall(McpToolCallItem {
                id: call_id.to_string(),
                server: "codex++".to_string(),
                tool: tool_name.to_string(),
                arguments: serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
                mcp_app_resource_uri: None,
                plugin_id: None,
                status: McpToolCallStatus::Completed,
                result: Some(CallToolResult {
                    content: Vec::new(),
                    structured_content: None,
                    is_error: Some(false),
                    meta: None,
                }),
                error: None,
                duration: None,
            }),
        )
        .await;
}

async fn emit_simple_tool_failed<T: serde::Serialize>(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    tool_name: &str,
    args: &T,
    message: &str,
) {
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::McpToolCall(McpToolCallItem {
                id: call_id.to_string(),
                server: "codex++".to_string(),
                tool: tool_name.to_string(),
                arguments: serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
                mcp_app_resource_uri: None,
                plugin_id: None,
                status: McpToolCallStatus::Failed,
                result: None,
                error: Some(McpToolCallError {
                    message: message.to_string(),
                }),
                duration: None,
            }),
        )
        .await;
}

#[derive(Debug)]
struct GlobMatcher {
    regex: Regex,
}

impl GlobMatcher {
    fn new(pattern: &str) -> Result<Self, FunctionCallError> {
        let regex = Regex::new(&glob_to_regex(pattern)).map_err(|error| {
            FunctionCallError::RespondToModel(format!("invalid glob pattern `{pattern}`: {error}"))
        })?;
        Ok(Self { regex })
    }

    fn is_match(&self, path: &str) -> bool {
        self.regex.is_match(path)
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    regex.push_str("(?:.*/)?");
                } else {
                    regex.push_str(".*");
                }
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '\\' => regex.push('/'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' => {
                regex.push('\\');
                regex.push(ch);
            }
            '/' => regex.push('/'),
            ch => regex.push(ch),
        }
    }
    regex.push('$');
    regex
}

#[cfg(test)]
#[path = "simple_search_tools_tests.rs"]
mod tests;
