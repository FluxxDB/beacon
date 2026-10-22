//! The error type shared by the server, CLI and TUI, and its process exit codes.

/// Every failure a beacon command can end in. [`Error::exit_code`] maps each
/// variant to the CLI's documented exit status.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bad arguments or input the user can fix (exit 1).
    #[error("{0}")]
    Usage(String),
    /// A channel, post or attachment that does not exist (exit 2).
    #[error("{0}")]
    NotFound(String),
    /// An id suffix that matches several posts; carries the candidates (exit 5).
    #[error("ambiguous id, matches: {}", .0.join(", "))]
    Ambiguous(Vec<String>),
    /// The server (or a peer) could not be reached: nothing accepted the
    /// connection (exit 3). The CLI adds the "start the node" hint to this.
    #[error("{0}")]
    Unreachable(String),
    /// A node that was reached stopped answering: the connection broke, or a
    /// request ran out of time (exit 3). The node is running, so no "start
    /// the node" hint; a request that may have been carried out says so.
    #[error("{0}")]
    Interrupted(String),
    /// The request was rejected for a missing or wrong secret (exit 4).
    #[error("unauthorized: wrong or missing BEACON_SECRET")]
    Auth,
    /// The request conflicts with existing state (exit 5).
    #[error("{0}")]
    Conflict(String),
    /// `wait` ran out of time before anything arrived (exit 6).
    #[error("timed out")]
    Timeout,
    /// Anything unexpected (exit 1).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl Error {
    /// The process exit status for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 1,
            Error::NotFound(_) => 2,
            Error::Ambiguous(_) => 5,
            Error::Unreachable(_) | Error::Interrupted(_) => 3,
            Error::Auth => 4,
            Error::Conflict(_) => 5,
            Error::Timeout => 6,
            Error::Internal(_) => 1,
        }
    }
}

impl From<rusqlite::Error> for Error {
    /// A database failure is never the user's doing.
    fn from(e: rusqlite::Error) -> Self {
        Error::Internal(e.into())
    }
}
