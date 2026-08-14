//! Request execution: credentials, signing, endpoints, HTTP.

pub mod credentials;
pub mod endpoint;
pub mod http;
pub mod presign;
pub mod retry;
pub mod rules;
pub mod sigv4;

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
    /// Client-side parameter validation, reported the way the reference formats it.
    #[error("An error occurred (ParamValidation): {0}")]
    ParamValidation(String),
    /// A modelled service error, reported the way the reference formats it.
    #[error("An error occurred ({code}) when calling the {operation} operation: {message}")]
    Service { code: String, message: String, operation: String },
}
