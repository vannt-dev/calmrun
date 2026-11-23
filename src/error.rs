//! The one error type. Every message says what went wrong and what to do next, because the
//! reader is often a coding agent that acts on the text.

use std::io;
use std::path::PathBuf;

/// Errors calmrun reports to the user.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The state directory could not be created or opened.
    #[error(
        "cannot open the state directory {path}: {source}\n\
         calmrun: next: make sure it is writable, or point CALMRUN_HOME somewhere that is"
    )]
    StateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The OS lock on `state.lock`, or reading/writing `state.json`, failed.
    #[error(
        "cannot lock or update the queue state: {0}\n\
         calmrun: next: check the state directory (`calmrun doctor` shows it)"
    )]
    State(#[source] io::Error),

    /// The command could not be started.
    #[error("cannot run {program}: {source}{}", spawn_hint(source))]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },

    /// A `--id` / MCP `queue` value that is not a safe directory name.
    #[error("invalid queue id \"{0}\": use letters, digits, '-', '_' or '.' (not first)")]
    InvalidQueue(String),

    /// A `--needs` / `hold` resource name that is not a plain name.
    #[error("invalid resource name \"{0}\": use letters, digits, '-', '_' or '.' (not first)")]
    InvalidResource(String),

    /// The OS did not provide system information (RAM, processes).
    #[error("cannot read system information: {0}")]
    SystemInfo(String),

    /// A file could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn spawn_hint(e: &io::Error) -> &'static str {
    if e.kind() == io::ErrorKind::NotFound {
        "\ncalmrun: next: check the name and PATH; a package's own bin may need `npx`, \
         `pnpm exec` or `uv run` in front"
    } else {
        ""
    }
}

impl Error {
    /// The exit code calmrun uses for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Spawn { source, .. } if source.kind() == io::ErrorKind::NotFound => 127,
            Error::Spawn { .. } => 126,
            Error::InvalidQueue(_) | Error::InvalidResource(_) => 2,
            _ => 1,
        }
    }

    /// The status word written to `history.jsonl`.
    pub fn status(&self) -> &'static str {
        match self {
            Error::Spawn { .. } => "spawn-failed",
            _ => "error",
        }
    }
}

/// Result with the crate error.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_program_is_127_with_a_hint() {
        let e = Error::Spawn {
            program: "vitestt".into(),
            source: io::Error::new(io::ErrorKind::NotFound, "command not found: vitestt"),
        };
        assert_eq!(e.exit_code(), 127);
        let text = e.to_string();
        assert!(text.starts_with("cannot run vitestt: command not found: vitestt\ncalmrun: next:"));
        assert_eq!(Error::InvalidQueue("../x".into()).exit_code(), 2);
    }
}
