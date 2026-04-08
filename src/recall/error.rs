//! Typed errors with stable exit codes for the recall subsystem.
//!
//! See spec §8.9 (R-480) and §14 (R-1001..R-1003). Errors propagate through
//! `anyhow::Error` and are downcast at the CLI boundary to map to exit codes.

use std::fmt;

/// Stable exit codes. Cf. spec R-480.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    Generic = 1,
    Constraint = 2,
    SchemaVersion = 3,
    NotInitialized = 4,
    ModelNotAvailable = 5,
    UsageError = 64,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// A recall error carries a message plus an exit code.
#[derive(Debug)]
pub struct RecallError {
    pub code: ExitCode,
    pub message: String,
}

impl RecallError {
    pub fn new(code: ExitCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            message: msg.into(),
        }
    }

    pub fn not_initialized() -> Self {
        Self::new(
            ExitCode::NotInitialized,
            "palace not initialized; run `ndx recall init`",
        )
    }

    pub fn constraint(msg: impl Into<String>) -> Self {
        Self::new(ExitCode::Constraint, msg)
    }

    pub fn schema_version(msg: impl Into<String>) -> Self {
        Self::new(ExitCode::SchemaVersion, msg)
    }

    pub fn model_unavailable(msg: impl Into<String>) -> Self {
        Self::new(ExitCode::ModelNotAvailable, msg)
    }

    pub fn usage(msg: impl Into<String>) -> Self {
        Self::new(ExitCode::UsageError, msg)
    }
}

impl fmt::Display for RecallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RecallError {}
