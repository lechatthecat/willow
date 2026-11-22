use super::{ErrorCode, Status};
use std::borrow::Cow;
use std::fmt::Write;

pub(super) type VerificationResult<T> = Result<T, VerificationError>;

#[derive(Debug, Clone)]
pub(super) struct VerificationError {
    pub kind: FailureKind,
    pub message: Cow<'static, str>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum FailureKind {
    Unsupported,
    Infrastructure,
    Counterexample,
    VacuousPrecondition,
}

impl FailureKind {
    pub fn error(self, message: impl Into<Cow<'static, str>>) -> VerificationError {
        VerificationError {
            kind: self,
            message: message.into(),
        }
    }

    pub fn code(self) -> ErrorCode {
        match self {
            Self::Unsupported => ErrorCode::E2631,
            Self::Infrastructure => ErrorCode::E2632,
            Self::Counterexample | Self::VacuousPrecondition => ErrorCode::E2630,
        }
    }

    pub fn status(self) -> Status {
        match self {
            Self::Unsupported | Self::Infrastructure => Status::Unknown,
            Self::Counterexample => Status::Counterexample,
            Self::VacuousPrecondition => Status::VacuousPrecondition,
        }
    }
}

/// Keep one link per failed caller. Only an emitted report needs the full chain.
#[derive(Debug)]
pub(super) enum FunctionFailure<'a> {
    Direct(VerificationError),
    Dependency { name: &'a str, cause: usize },
}

impl From<VerificationError> for FunctionFailure<'_> {
    fn from(error: VerificationError) -> Self {
        Self::Direct(error)
    }
}

impl FunctionFailure<'_> {
    pub fn kind(&self) -> FailureKind {
        match self {
            Self::Direct(error) => error.kind,
            Self::Dependency { .. } => FailureKind::Unsupported,
        }
    }

    pub fn message(&self, failures: &[Self]) -> String {
        let mut text = String::new();
        let mut current = self;
        loop {
            match current {
                Self::Direct(error) => {
                    text.push_str(&error.message);
                    return text;
                }
                Self::Dependency { name, cause } => {
                    write!(text, "dependency `{name}` is unverified: ").unwrap();
                    current = &failures[*cause];
                }
            }
        }
    }
}
