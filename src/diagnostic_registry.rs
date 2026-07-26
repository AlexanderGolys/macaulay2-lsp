//! The single registry of every diagnostic the server emits.
//!
//! Each diagnostic is one [`M2Diagnostic`] variant carrying its stable `E..`
//! code, its slug name, and its severity. Detection logic lives where the
//! context is (it varies: some checks need the type registry, others only a
//! node), but every emission funnels through [`M2Diagnostic::at`] so the code
//! and severity are assigned in exactly one place. Code actions recover the
//! diagnostic they fix by [`diagnostic_has_kind`] — a code match, not the
//! former brittle message-string comparison.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range as LspRange};

/// Every diagnostic the server can publish. Each variant is forced through the
/// exhaustive `match`es below (code, name, severity), so a new diagnostic
/// cannot be half-registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum M2Diagnostic {
    SyntaxError,
    MissingNode,
    AmbiguousFloatMemberAccess,
    MultipleAssignmentTargets,
    ColonEqualPartAssignment,
    ParallelAssignmentArity,
    OptionKeyConvention,
    UnusedBinding,
    InstallNoEffect,
    OperatorNotFlexible,
    InstallArity,
    InstallNeedsColonEquals,
    ProtectAssignedSymbol,
    ProtectComputedSymbol,
}

impl M2Diagnostic {
    pub const ALL: [Self; 14] = [
        Self::SyntaxError,
        Self::MissingNode,
        Self::AmbiguousFloatMemberAccess,
        Self::MultipleAssignmentTargets,
        Self::ColonEqualPartAssignment,
        Self::ParallelAssignmentArity,
        Self::OptionKeyConvention,
        Self::UnusedBinding,
        Self::InstallNoEffect,
        Self::OperatorNotFlexible,
        Self::InstallArity,
        Self::InstallNeedsColonEquals,
        Self::ProtectAssignedSymbol,
        Self::ProtectComputedSymbol,
    ];

    /// The stable `E..` code surfaced to the editor (rustc-style).
    pub fn code(self) -> &'static str {
        match self {
            Self::SyntaxError => "E00",
            Self::MissingNode => "E01",
            Self::AmbiguousFloatMemberAccess => "E02",
            Self::MultipleAssignmentTargets => "E03",
            Self::ColonEqualPartAssignment => "E04",
            Self::ParallelAssignmentArity => "E05",
            Self::OptionKeyConvention => "E06",
            Self::UnusedBinding => "E07",
            Self::InstallNoEffect => "E08",
            Self::OperatorNotFlexible => "E09",
            Self::InstallArity => "E10",
            Self::InstallNeedsColonEquals => "E11",
            Self::ProtectAssignedSymbol => "E12",
            Self::ProtectComputedSymbol => "E13",
        }
    }

    /// The human-readable rule name, published as the diagnostic `source` so the
    /// editor shows which rule fired alongside the `E..` code.
    pub fn name(self) -> &'static str {
        match self {
            Self::SyntaxError => "syntax-error",
            Self::MissingNode => "missing-node",
            Self::AmbiguousFloatMemberAccess => "ambiguous-float-member-access",
            Self::MultipleAssignmentTargets => "multiple-assignment-targets",
            Self::ColonEqualPartAssignment => "colon-equal-part-assignment",
            Self::ParallelAssignmentArity => "parallel-assignment-arity",
            Self::OptionKeyConvention => "option-key-convention",
            Self::UnusedBinding => "unused-binding",
            Self::InstallNoEffect => "install-no-effect",
            Self::OperatorNotFlexible => "operator-not-flexible",
            Self::InstallArity => "install-arity",
            Self::InstallNeedsColonEquals => "install-needs-colon-equals",
            Self::ProtectAssignedSymbol => "protect-assigned-symbol",
            Self::ProtectComputedSymbol => "protect-computed-symbol",
        }
    }

    pub fn severity(self) -> DiagnosticSeverity {
        match self {
            Self::SyntaxError
            | Self::MissingNode
            | Self::MultipleAssignmentTargets
            | Self::ColonEqualPartAssignment
            | Self::ParallelAssignmentArity
            | Self::OperatorNotFlexible
            | Self::InstallArity
            | Self::InstallNeedsColonEquals => DiagnosticSeverity::ERROR,
            Self::AmbiguousFloatMemberAccess | Self::UnusedBinding | Self::InstallNoEffect => {
                DiagnosticSeverity::WARNING
            }
            Self::ProtectComputedSymbol => DiagnosticSeverity::WARNING,
            Self::OptionKeyConvention | Self::ProtectAssignedSymbol => DiagnosticSeverity::HINT,
        }
    }

    /// Build the LSP diagnostic for this rule over `range` with `message`,
    /// stamping severity, the `E..` code, and the rule name as `source` from the
    /// registry so no call site re-specifies them.
    pub fn at(self, range: LspRange, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            range,
            severity: Some(self.severity()),
            code: Some(NumberOrString::String(self.code().to_string())),
            source: Some(self.name().to_string()),
            message: message.into(),
            ..Default::default()
        }
    }

    pub fn from_selector(selector: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|diagnostic| selector == diagnostic.code() || selector == diagnostic.name())
    }

    pub fn from_diagnostic(diagnostic: &Diagnostic) -> Option<Self> {
        let NumberOrString::String(code) = diagnostic.code.as_ref()? else {
            return None;
        };
        Self::ALL
            .into_iter()
            .find(|diagnostic| code == diagnostic.code())
    }
}

pub trait DiagnosticPolicy {
    fn allows(&self, diagnostic: M2Diagnostic) -> bool;

    fn allows_lsp_diagnostic(&self, diagnostic: &Diagnostic) -> bool {
        M2Diagnostic::from_diagnostic(diagnostic).is_none_or(|kind| self.allows(kind))
    }
}

/// Whether `diagnostic` was emitted for `kind`, matched by its stable code.
pub fn diagnostic_has_kind(diagnostic: &Diagnostic, kind: M2Diagnostic) -> bool {
    M2Diagnostic::from_diagnostic(diagnostic) == Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_contiguous_from_e00_and_names_are_unique() {
        // Every variant must appear here; the exhaustive `match`es guarantee a
        // new one is given a code/name/severity, and this list keeps the `E..`
        // codes contiguous and the names unique.
        let mut names = HashSet::new();
        for (index, diagnostic) in M2Diagnostic::ALL.iter().enumerate() {
            assert_eq!(
                diagnostic.code(),
                format!("E{index:02}"),
                "diagnostic codes must be contiguous E00.. in listing order"
            );
            assert!(
                names.insert(diagnostic.name()),
                "duplicate diagnostic name `{}`",
                diagnostic.name()
            );
        }
    }
}
