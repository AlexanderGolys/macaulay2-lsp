//! Diagnostic identities and metadata shared by analysis and LSP capabilities.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range as TextRange};

#[macro_export]
macro_rules! diagnostic_declarations {
    ($consumer:ident) => {
        $consumer! {
          diagnostics {
            SyntaxError {
                code: "X01", name: "syntax-error", severity: ERROR, legacy: ["E00"],
                check: node => |context| context.syntax_error(), action: none,
            },
            MissingNode {
                code: "X02", name: "missing-node", severity: ERROR, legacy: ["E01"],
                check: node => |context| context.missing_node(), action: none,
            },
            AmbiguousFloatMemberAccess {
                code: "X03", name: "ambiguous-float-member-access", severity: WARNING,
                legacy: ["E02"],
                check: node => |context| context.ambiguous_float_member_access(),
                action: (|context| ambiguous_float_member_access_action(context)),
            },
            MultipleAssignmentTargets {
                code: "X04", name: "multiple-assignment-targets", severity: ERROR,
                legacy: ["E03"],
                check: node => |context| context.multiple_assignment_targets(), action: none,
            },
            ColonEqualPartAssignment {
                code: "X05", name: "colon-equal-part-assignment", severity: ERROR,
                legacy: ["E04"],
                check: node => |context| context.colon_equal_part_assignment(),
                action: (|context| colon_equal_part_assignment_action(context)),
            },
            ParallelAssignmentArity {
                code: "X06", name: "parallel-assignment-arity", severity: ERROR,
                legacy: ["E05"],
                check: node => |context| context.parallel_assignment_arity(), action: none,
            },
            OptionKeyConvention {
                code: "S01", name: "option-key-convention", severity: HINT, legacy: ["E06"],
                check: node => |context| context.option_key_convention(),
                action: (|context| option_key_convention_action(context)),
            },
            UnusedBinding {
                code: "S02", name: "unused-binding", severity: WARNING, legacy: ["E07"],
                check: document => |context| context.unused_bindings(), action: none,
            },
            InstallNoEffect {
                code: "E01", name: "install-no-effect", severity: WARNING, legacy: ["E08"],
                check: installation => |context| context.install_no_effect(), action: none,
            },
            OperatorNotFlexible {
                code: "E02", name: "operator-not-flexible", severity: ERROR, legacy: ["E09"],
                check: installation => |context| context.operator_not_flexible(), action: none,
            },
            InstallArity {
                code: "E03", name: "install-arity", severity: ERROR, legacy: ["E10"],
                check: installation => |context| context.install_arity(), action: none,
            },
            InstallNeedsColonEquals {
                code: "E04", name: "install-needs-colon-equals", severity: ERROR,
                legacy: ["E11"],
                check: node => |context| context.install_needs_colon_equals(),
                action: (|context| install_needs_colon_equals_action(context)),
            },
            ProtectAssignedSymbol {
                code: "E05", name: "protect-assigned-symbol", severity: HINT, legacy: ["E12"],
                check: node => |context| context.protect_assigned_symbol(),
                action: (|context| protect_assigned_symbol_action(context)),
            },
            ProtectComputedSymbol {
                code: "E06", name: "protect-computed-symbol", severity: WARNING,
                legacy: ["E13"],
                check: node => |context| context.protect_computed_symbol(), action: none,
            },
            MissingOutputCell {
                code: "E07", name: "missing-output-cell", severity: WARNING, legacy: ["E14"],
                check: node => |context| context.missing_output_cell(), action: none,
            },
            InvalidControlTransfer {
                code: "E08", name: "invalid-control-transfer", severity: ERROR, legacy: ["E15"],
                check: node => |context| context.invalid_control_transfer(), action: none,
            },
            ParallelAssignmentType {
                code: "T01", name: "parallel-assignment-type", severity: ERROR,
                legacy: ["E16"],
                check: node => |context| context.parallel_assignment_type(), action: none,
            },
            ConditionType {
                code: "T02", name: "condition-type", severity: WARNING,
                legacy: ["E17", "E18", "while-condition-type", "if-condition-type"],
                check: node => |context| context.condition_type(), action: none,
            },
            InstallCodomainMissing {
                code: "T03", name: "install-codomain-missing", severity: HINT,
                legacy: ["E19"],
                check: codomain => |context| context.missing_codomain(),
                action: (|context| method_codomain_action(context)),
            },
            InstallCodomainMismatch {
                code: "T04", name: "install-codomain-mismatch", severity: WARNING,
                legacy: ["E20"],
                check: codomain => |context| context.codomain_mismatch(), action: none,
            },
          }
          standalone_actions {
            ConvertToRawString => |context| convert_to_raw_string_action(context),
            ConditionalNull => |context| conditional_null_action(context),
            SimplifyTry => |context| simplify_try_action(context),
            SimplifyIfCondition => |context| simplify_if_condition_action(context),
            FlattenElseIf => |context| flatten_else_if_action(context),
          }
        }
    };
}

struct DiagnosticRegistration {
    code: &'static str,
    name: &'static str,
    severity: DiagnosticSeverity,
    legacy_selectors: &'static [&'static str],
}

macro_rules! register_diagnostics {
    (diagnostics { $(
        $kind:ident {
            code: $code:literal, name: $name:literal, severity: $severity:ident,
            legacy: [$($legacy:literal),* $(,)?],
            check: $phase:ident => |$context:ident| $check:expr, action: $action:tt,
        }
    ),+ $(,)? } standalone_actions { $($standalone:ident => |$action_context:ident| $standalone_action:expr),* $(,)? }) => {
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
                        legacy_selectors: &[$($legacy),*],
                    }),+
                }
            }
        }
    };
}

diagnostic_declarations!(register_diagnostics);

impl DiagnosticKind {
    pub fn at(self, range: TextRange, message: impl Into<String>) -> M2Diagnostic {
        M2Diagnostic {
            kind: self,
            range,
            message: message.into(),
        }
    }

    pub fn from_selector(selector: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|diagnostic| {
                let registration = diagnostic.registration();
                selector == registration.code || selector == registration.name
            })
            .or_else(|| {
                Self::ALL.iter().copied().find(|diagnostic| {
                    diagnostic
                        .registration()
                        .legacy_selectors
                        .contains(&selector)
                })
            })
    }

    pub fn from_lsp(diagnostic: &Diagnostic) -> Option<Self> {
        let NumberOrString::String(code) = diagnostic.code.as_ref()? else {
            return None;
        };
        Self::from_selector(code)
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

pub fn diagnostic_has_kind(diagnostic: &Diagnostic, kind: DiagnosticKind) -> bool {
    DiagnosticKind::from_lsp(diagnostic) == Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn categorized_codes_names_and_legacy_selectors_are_unique() {
        let mut codes = HashSet::new();
        let mut names = HashSet::new();
        let mut legacy_selectors = HashSet::new();
        for diagnostic in DiagnosticKind::ALL {
            let registration = diagnostic.registration();
            assert!(
                matches!(
                    registration.code.as_bytes(),
                    [b'D' | b'T' | b'E' | b'X' | b'S', b'0'..=b'9', b'0'..=b'9']
                ),
                "invalid categorized diagnostic code `{}`",
                registration.code
            );
            assert!(codes.insert(registration.code), "duplicate diagnostic code");
            assert!(names.insert(registration.name), "duplicate diagnostic name");
            for selector in registration.legacy_selectors {
                assert!(
                    legacy_selectors.insert(*selector),
                    "duplicate legacy diagnostic selector `{selector}`"
                );
            }
        }
    }

    #[test]
    fn canonical_codes_take_precedence_over_colliding_legacy_selectors() {
        assert_eq!(
            DiagnosticKind::from_selector("E01"),
            Some(DiagnosticKind::InstallNoEffect)
        );
        assert_eq!(
            DiagnosticKind::from_selector("E20"),
            Some(DiagnosticKind::InstallCodomainMismatch)
        );
        assert_eq!(
            DiagnosticKind::from_selector("E00"),
            Some(DiagnosticKind::SyntaxError)
        );
    }
}
