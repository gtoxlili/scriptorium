use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("workspace_id is empty or invalid")]
    InvalidWorkspaceId,

    #[error("path escapes workspace root: {0}")]
    PathEscape(String),

    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    #[error("docker: {0}")]
    Docker(#[from] bollard::errors::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("exec timed out after {0}s")]
    ExecTimeout(u32),

    #[error("{0}")]
    Other(String),
}

impl From<Error> for tonic::Status {
    fn from(err: Error) -> Self {
        use tonic::Code;
        let code = match &err {
            Error::InvalidWorkspaceId | Error::PathEscape(_) => Code::InvalidArgument,
            Error::WorkspaceNotFound(_) => Code::NotFound,
            Error::ExecTimeout(_) => Code::DeadlineExceeded,
            Error::Docker(_) | Error::Io(_) | Error::Other(_) => Code::Internal,
        };
        tonic::Status::new(code, err.to_string())
    }
}
