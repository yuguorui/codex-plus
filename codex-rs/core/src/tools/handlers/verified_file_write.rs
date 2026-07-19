use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::ReadFileOptions;
use codex_exec_server::WriteFileOptions;
use codex_utils_path_uri::PathUri;
use std::io;

pub(crate) enum ExpectedFileContents<'a> {
    Missing,
    Present(&'a [u8]),
}

impl ExpectedFileContents<'_> {
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Missing => None,
            Self::Present(contents) => Some(contents),
        }
    }
}

pub(crate) struct VerifiedFileWrite<'a> {
    pub fs: &'a dyn ExecutorFileSystem,
    pub path: &'a PathUri,
    pub sandbox: Option<&'a FileSystemSandboxContext>,
    pub expected: ExpectedFileContents<'a>,
    pub updated: &'a [u8],
}

#[derive(Debug)]
pub(crate) enum VerifiedFileWriteError {
    ReadBeforeWrite(io::Error),
    UnexpectedContents,
    CreateParent(io::Error),
    Write(io::Error),
    ReadAfterWrite(io::Error),
    UnexpectedWrittenContents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileWriteCommitState {
    NotCommitted,
    Committed,
    Unknown,
}

impl VerifiedFileWriteError {
    pub(crate) fn commit_state(&self) -> FileWriteCommitState {
        match self {
            Self::ReadBeforeWrite(_) | Self::UnexpectedContents | Self::CreateParent(_) => {
                FileWriteCommitState::NotCommitted
            }
            Self::ReadAfterWrite(_) => FileWriteCommitState::Committed,
            Self::Write(_) | Self::UnexpectedWrittenContents => FileWriteCommitState::Unknown,
        }
    }
}

pub(crate) fn verified_write_error_message(
    error: &VerifiedFileWriteError,
    tool_name: &str,
    path_summary: &str,
) -> String {
    match error {
        VerifiedFileWriteError::ReadBeforeWrite(error) => {
            format!("{tool_name} failed to re-read `{path_summary}` before writing: {error}")
        }
        VerifiedFileWriteError::UnexpectedContents => {
            "File has been unexpectedly modified. Read it again before attempting to write it."
                .to_string()
        }
        VerifiedFileWriteError::CreateParent(error) => {
            format!("{tool_name} failed to create parent directories: {error}")
        }
        VerifiedFileWriteError::Write(error) => {
            format!("{tool_name} failed to write `{path_summary}`: {error}")
        }
        VerifiedFileWriteError::ReadAfterWrite(error) => {
            format!("{tool_name} could not verify the written file `{path_summary}`: {error}")
        }
        VerifiedFileWriteError::UnexpectedWrittenContents => format!(
            "{tool_name} did not produce the expected contents for `{path_summary}`. Read the file again before retrying."
        ),
    }
}

pub(crate) async fn write_file_verified(
    request: VerifiedFileWrite<'_>,
) -> Result<(), VerifiedFileWriteError> {
    let current = match request
        .fs
        .read_file(request.path, ReadFileOptions::default(), request.sandbox)
        .await
    {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(VerifiedFileWriteError::ReadBeforeWrite(error)),
    };
    if current.as_deref() != request.expected.as_bytes() {
        return Err(VerifiedFileWriteError::UnexpectedContents);
    }

    if matches!(&request.expected, ExpectedFileContents::Missing)
        && let Some(parent) = request.path.parent()
    {
        request
            .fs
            .create_directory(
                &parent,
                CreateDirectoryOptions {
                    recursive: true,
                    follow_symlinks: true,
                },
                request.sandbox,
            )
            .await
            .map_err(VerifiedFileWriteError::CreateParent)?;
    }

    request
        .fs
        .write_file(
            request.path,
            request.updated.to_vec(),
            WriteFileOptions::default(),
            request.sandbox,
        )
        .await
        .map_err(VerifiedFileWriteError::Write)?;
    let written = request
        .fs
        .read_file(request.path, ReadFileOptions::default(), request.sandbox)
        .await
        .map_err(VerifiedFileWriteError::ReadAfterWrite)?;
    if written != request.updated {
        return Err(VerifiedFileWriteError::UnexpectedWrittenContents);
    }
    Ok(())
}

#[cfg(test)]
#[path = "verified_file_write_tests.rs"]
mod tests;
