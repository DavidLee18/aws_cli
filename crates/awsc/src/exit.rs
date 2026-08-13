//! Exit codes, matching the reference exactly.
//!
//! From `awscli/constants.py:14-17`, verified by running the reference:
//!
//! | code | meaning | example |
//! |---|---|---|
//! | 0 | success, including `help` | `aws help` |
//! | 252 | parameter validation | unknown service/operation/flag, no args |
//! | 253 | configuration | credentials could not be resolved |
//! | 254 | client/service error | `InvalidClientTokenId`, missing region |
//! | 255 | general error | anything else |
//!
//! Scripts branch on these, so a drop-in replacement has to reproduce them — note in
//! particular that a missing region is 254 (a client error), not 253, and that bare
//! `aws` with no arguments is 252 rather than 0.

use std::process::ExitCode;

pub const SUCCESS: u8 = 0;
pub const PARAM_VALIDATION: u8 = 252;
pub const CONFIGURATION: u8 = 253;
pub const CLIENT_ERROR: u8 = 254;
pub const GENERAL_ERROR: u8 = 255;

pub fn code(value: u8) -> ExitCode {
    ExitCode::from(value)
}
