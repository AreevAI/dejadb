//! Waiser error type. Follows the DejaDB error convention: every variant's
//! `Display` string leads with a stable `WSR-Ennn` code, and `code()` returns
//! it. Codes are **append-only** — never renumber or reuse (see
//! `ERROR_CODES.md`). The engine has zero dejadb dependencies, so it owns its
//! own domain (`WSR`); REVIEW/APPLY *syntax* errors belong to the substrate's
//! CAL domain, not here.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// All errors produced by the Waiser engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A substrate call (grain read/write, CAL execution) failed.
    Substrate(String),
    /// The substrate cannot execute the given CAL (e.g. the reference
    /// substrate's minimal subset). Distinct from a substrate fault.
    CalUnsupported(String),
    /// A `target_ref` did not parse to a known scheme.
    InvalidTargetRef(String),
    /// A proposal payload failed structural validation.
    InvalidProposal(String),
    /// A recommendation draft is missing a MUST field or is malformed.
    InvalidRecommendation(String),
    /// An attempted lifecycle transition is not allowed from the current state.
    LifecycleViolation(String),
    /// Self-approval blocked: the approving actor authored the recommendation.
    SelfApproval(String),
    /// The caller lacks a scope required for the operation.
    ScopeDenied(String),
    /// A destructive apply was attempted without the required gates.
    DestructiveGated(String),
    /// One analyzer's run failed; its findings are dropped, the run continues.
    AnalyzerFailed { id: String, message: String },
    /// An analyzer parameter is outside its declared `ParamSpec`.
    ParamInvalid(String),
    /// A required substrate capability (forks, telemetry, embeddings) is absent.
    CapabilityMissing(String),
    /// The optional LLM enrichment backend (`--llm-cmd`) is misconfigured or
    /// failed. Never fatal to a run — the LLM contribution is dropped.
    LlmBackend(String),
    /// No recommendation exists at the given hash.
    NotFound(String),
    /// An unexpected internal fault (should not happen — file a bug).
    Internal(String),
}

impl Error {
    /// Stable machine-readable code in `WSR-Ennn` form.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Substrate(_) => "WSR-E001",
            Error::CalUnsupported(_) => "WSR-E002",
            Error::InvalidTargetRef(_) => "WSR-E010",
            Error::InvalidProposal(_) => "WSR-E011",
            Error::InvalidRecommendation(_) => "WSR-E012",
            Error::LifecycleViolation(_) => "WSR-E020",
            Error::SelfApproval(_) => "WSR-E021",
            Error::ScopeDenied(_) => "WSR-E022",
            Error::DestructiveGated(_) => "WSR-E023",
            Error::AnalyzerFailed { .. } => "WSR-E030",
            Error::ParamInvalid(_) => "WSR-E031",
            Error::CapabilityMissing(_) => "WSR-E032",
            Error::LlmBackend(_) => "WSR-E050",
            Error::NotFound(_) => "WSR-E040",
            Error::Internal(_) => "WSR-E099",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Error::Substrate(m) => write!(f, "{code} substrate call failed: {m}"),
            Error::CalUnsupported(m) => write!(f, "{code} CAL not supported by substrate: {m}"),
            Error::InvalidTargetRef(m) => write!(f, "{code} invalid target_ref: {m}"),
            Error::InvalidProposal(m) => write!(f, "{code} invalid proposal: {m}"),
            Error::InvalidRecommendation(m) => write!(f, "{code} invalid recommendation: {m}"),
            Error::LifecycleViolation(m) => write!(f, "{code} illegal lifecycle transition: {m}"),
            Error::SelfApproval(m) => write!(f, "{code} self-approval blocked: {m}"),
            Error::ScopeDenied(m) => write!(f, "{code} scope denied: {m}"),
            Error::DestructiveGated(m) => write!(f, "{code} destructive apply gated: {m}"),
            Error::AnalyzerFailed { id, message } => {
                write!(f, "{code} analyzer {id} failed: {message}")
            }
            Error::ParamInvalid(m) => write!(f, "{code} analyzer parameter invalid: {m}"),
            Error::CapabilityMissing(m) => write!(f, "{code} required capability missing: {m}"),
            Error::LlmBackend(m) => write!(f, "{code} LLM backend error: {m}"),
            Error::NotFound(m) => write!(f, "{code} recommendation not found: {m}"),
            Error::Internal(m) => write!(f, "{code} internal error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_leads_with_code() {
        let e = Error::InvalidTargetRef("nope".into());
        assert!(e.to_string().starts_with("WSR-E010 "));
        assert_eq!(e.code(), "WSR-E010");
    }

    #[test]
    fn codes_are_unique_and_well_formed() {
        // One representative per variant; guards against copy-paste collisions.
        let all = [
            Error::Substrate(String::new()),
            Error::CalUnsupported(String::new()),
            Error::InvalidTargetRef(String::new()),
            Error::InvalidProposal(String::new()),
            Error::InvalidRecommendation(String::new()),
            Error::LifecycleViolation(String::new()),
            Error::SelfApproval(String::new()),
            Error::ScopeDenied(String::new()),
            Error::DestructiveGated(String::new()),
            Error::AnalyzerFailed {
                id: String::new(),
                message: String::new(),
            },
            Error::ParamInvalid(String::new()),
            Error::CapabilityMissing(String::new()),
            Error::NotFound(String::new()),
            Error::Internal(String::new()),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for e in &all {
            let c = e.code();
            assert!(c.starts_with("WSR-E"), "bad domain: {c}");
            assert_eq!(c.len(), 8, "bad code length: {c}");
            assert!(seen.insert(c), "duplicate code: {c}");
        }
    }
}
