//! The diagnostics emitted by the server.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range as TextRange};

struct DiagnosticRegistration {
    code: &'static str,
    name: &'static str,
    severity: DiagnosticSeverity,
}

macro_rules! register_diagnostics {
    ($($kind:ident => ($code:literal, $name:literal, $severity:ident)),+ $(,)?) => {
        /// A diagnostic the server can publish.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum DiagnosticKind {
            $($kind),+
        }

        impl DiagnosticKind {
            pub const ALL: &'static [Self] = &[$(Self::$kind),+];

            fn registration(self) -> DiagnosticRegistration {
                match self {
                    $(Self::$kind => DiagnosticRegistration {
                        code: $code,
                        name: $name,
                        severity: DiagnosticSeverity::$severity,
                    }),+
                }
            }
        }
    };
}

register_diagnostics! {
    SyntaxError => ("E00", "syntax-error", ERROR),
    MissingNode => ("E01", "missing-node", ERROR),
    AmbiguousFloatMemberAccess => ("E02", "ambiguous-float-member-access", WARNING),
    MultipleAssignmentTargets => ("E03", "multiple-assignment-targets", ERROR),
    ColonEqualPartAssignment => ("E04", "colon-equal-part-assignment", ERROR),
    ParallelAssignmentArity => ("E05", "parallel-assignment-arity", ERROR),
    OptionKeyConvention => ("E06", "option-key-convention", HINT),
    UnusedBinding => ("E07", "unused-binding", WARNING),
    InstallNoEffect => ("E08", "install-no-effect", WARNING),
    OperatorNotFlexible => ("E09", "operator-not-flexible", ERROR),
    InstallArity => ("E10", "install-arity", ERROR),
    InstallNeedsColonEquals => ("E11", "install-needs-colon-equals", ERROR),
    ProtectAssignedSymbol => ("E12", "protect-assigned-symbol", HINT),
    ProtectComputedSymbol => ("E13", "protect-computed-symbol", WARNING),
    MissingOutputCell => ("E14", "missing-output-cell", WARNING),
    InvalidControlTransfer => ("E15", "invalid-control-transfer", ERROR),
    ParallelAssignmentType => ("E16", "parallel-assignment-type", ERROR),
    WhileConditionType => ("E17", "while-condition-type", ERROR),
    IfConditionType => ("E18", "if-condition-type", ERROR),
    InstallCodomainMissing => ("E19", "install-codomain-missing", HINT),
    InstallCodomainMismatch => ("E20", "install-codomain-mismatch", WARNING),
}

impl DiagnosticKind {
    pub fn at(self, range: TextRange, message: impl Into<String>) -> M2Diagnostic {
        M2Diagnostic {
            kind: self,
            range,
            message: message.into(),
        }
    }

    pub fn from_selector(selector: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|diagnostic| {
            let registration = diagnostic.registration();
            selector == registration.code || selector == registration.name
        })
    }

    pub fn from_lsp(diagnostic: &Diagnostic) -> Option<Self> {
        let NumberOrString::String(code) = diagnostic.code.as_ref()? else {
            return None;
        };
        Self::ALL
            .iter()
            .copied()
            .find(|diagnostic| code == diagnostic.registration().code)
    }
}

/// One typed diagnostic finding produced by source analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M2Diagnostic {
    pub kind: DiagnosticKind,
    pub range: TextRange,
    pub message: String,
}

impl M2Diagnostic {
    pub fn to_lsp(&self) -> Diagnostic {
        let registration = self.kind.registration();
        Diagnostic {
            range: self.range,
            severity: Some(registration.severity),
            code: Some(NumberOrString::String(registration.code.to_string())),
            source: Some(registration.name.to_string()),
            message: self.message.clone(),
            ..Default::default()
        }
    }
}

pub trait DiagnosticPolicy {
    fn allows(&self, diagnostic: DiagnosticKind) -> bool;

    fn allows_lsp_diagnostic(&self, diagnostic: &Diagnostic) -> bool {
        DiagnosticKind::from_lsp(diagnostic).is_none_or(|kind| self.allows(kind))
    }
}

/// Whether `diagnostic` was emitted for `kind`, matched by its stable code.
pub fn diagnostic_has_kind(diagnostic: &Diagnostic, kind: DiagnosticKind) -> bool {
    DiagnosticKind::from_lsp(diagnostic) == Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_contiguous_from_e00_and_names_are_unique() {
        let mut names = HashSet::new();
        for (index, diagnostic) in DiagnosticKind::ALL.iter().enumerate() {
            let registration = diagnostic.registration();
            assert_eq!(
                registration.code,
                format!("E{index:02}"),
                "diagnostic codes must be contiguous E00.. in listing order"
            );
            assert!(
                names.insert(registration.name),
                "duplicate diagnostic name `{}`",
                registration.name
            );
        }
    }
}
