//! Request execution: credentials, signing, endpoints, HTTP.

pub mod credentials;
pub mod endpoint;
pub mod http;
pub mod localtime;
pub mod presign;
pub mod retry;
pub mod rules;
pub mod sigv4;
pub mod transport;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Credentials(#[from] credentials::CredentialError),
    /// Worded to match the reference byte-for-byte (verified against
    /// `aws ec2 describe-regions` with no region configured).
    #[error(
        "An error occurred (NoRegion): You must specify a region. \
         You can also configure your region by running \"aws configure\"."
    )]
    NoRegion,
    #[error("network error: {0}")]
    Http(String),
    /// A non-2xx response on a streaming path, where the body was never written to the
    /// sink. Carries the error document so the caller can parse a modelled error out of
    /// it, and the headers because S3 reports some failures only there.
    #[error("HTTP {status}")]
    HttpStatus { status: u16, body: String, headers: Vec<(String, String)> },
    /// Client-side parameter validation, reported the way the reference formats it.
    #[error("An error occurred (ParamValidation): {0}")]
    ParamValidation(String),
    /// A configuration problem the CLI itself raises -- a missing SSO setting, an
    /// sso-session that is not defined. Distinct from botocore's own `ProfileNotFound`,
    /// which reaches the general handler and carries no such prefix.
    #[error("An error occurred (Configuration): {0}")]
    Configuration(String),
    /// A modelled service error, reported the way the reference formats it.
    #[error("An error occurred ({code}) when calling the {operation} operation: {message}")]
    Service { code: String, message: String, operation: String },
}
