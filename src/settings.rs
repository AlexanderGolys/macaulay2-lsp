use std::collections::HashSet;
use std::sync::RwLock;

use serde::{de::Error as _, Deserialize, Deserializer};
use serde_json::Value;

use crate::capabilities::formatting::FormattingConfiguration;
use crate::diagnostic_registry::{DiagnosticPolicy, M2Diagnostic};

#[derive(Debug)]
pub(crate) struct SettingsStore<T> {
    current: RwLock<T>,
}

impl<T: Default> Default for SettingsStore<T> {
    fn default() -> Self {
        Self {
            current: RwLock::new(T::default()),
        }
    }
}

impl<T: Clone> SettingsStore<T> {
    pub(crate) fn snapshot(&self) -> T {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl<T> SettingsStore<T> {
    pub(crate) fn replace(&self, value: T) {
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ServerSettings {
    diagnostics: DiagnosticSettings,
    formatting: FormattingSettings,
    inlay_hints: InlayHintSettings,
}

impl ServerSettings {
    pub(crate) fn from_value(value: &Value) -> serde_json::Result<Self> {
        let settings = value
            .get("m2-ls")
            .or_else(|| value.get("macaulay2"))
            .unwrap_or(value);
        serde_json::from_value(settings.clone())
    }

    pub(crate) fn diagnostics(&self) -> &DiagnosticSettings {
        &self.diagnostics
    }

    pub(crate) fn formatting(&self) -> &FormattingSettings {
        &self.formatting
    }

    pub(crate) fn expression_type_hints(&self) -> bool {
        self.inlay_hints.expression_types
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct DiagnosticSettings {
    enabled: bool,
    #[serde(deserialize_with = "deserialize_diagnostic_set")]
    disabled: HashSet<M2Diagnostic>,
}

impl Default for DiagnosticSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: HashSet::new(),
        }
    }
}

impl DiagnosticPolicy for DiagnosticSettings {
    fn allows(&self, diagnostic: M2Diagnostic) -> bool {
        self.enabled && !self.disabled.contains(&diagnostic)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct FormattingSettings {
    indent_width: Option<u32>,
    use_tabs: Option<bool>,
    compact_factor_operators: bool,
    break_after_semicolon: bool,
}

impl Default for FormattingSettings {
    fn default() -> Self {
        Self {
            indent_width: None,
            use_tabs: None,
            compact_factor_operators: false,
            break_after_semicolon: true,
        }
    }
}

impl FormattingConfiguration for FormattingSettings {
    fn indent_width(&self) -> Option<u32> {
        self.indent_width
    }

    fn use_tabs(&self) -> Option<bool> {
        self.use_tabs
    }

    fn compact_factor_operators(&self) -> bool {
        self.compact_factor_operators
    }

    fn break_after_semicolon(&self) -> bool {
        self.break_after_semicolon
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct InlayHintSettings {
    expression_types: bool,
}

fn deserialize_diagnostic_set<'de, D>(deserializer: D) -> Result<HashSet<M2Diagnostic>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|selector| {
            M2Diagnostic::from_selector(&selector)
                .ok_or_else(|| D::Error::custom(format!("unknown diagnostic `{selector}`")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_enable_diagnostics_and_canonical_factor_spacing() {
        let settings = ServerSettings::from_value(&serde_json::json!({})).unwrap();

        assert!(settings.diagnostics().allows(M2Diagnostic::UnusedBinding));
        assert!(!settings.formatting().compact_factor_operators());
        assert!(settings.formatting().break_after_semicolon());
        assert!(!settings.expression_type_hints());
    }

    #[test]
    fn parses_nested_editor_settings_and_diagnostic_selectors() {
        let settings = ServerSettings::from_value(&serde_json::json!({
            "m2-ls": {
                "diagnostics": {
                    "disabled": ["unused-binding", "E06"]
                },
                "formatting": {
                    "indentWidth": 2,
                    "useTabs": false,
                    "compactFactorOperators": true,
                    "breakAfterSemicolon": false
                },
                "inlayHints": {
                    "expressionTypes": true
                }
            }
        }))
        .unwrap();

        assert!(!settings.diagnostics().allows(M2Diagnostic::UnusedBinding));
        assert!(!settings
            .diagnostics()
            .allows(M2Diagnostic::OptionKeyConvention));
        assert!(settings.diagnostics().allows(M2Diagnostic::SyntaxError));
        assert_eq!(settings.formatting().indent_width(), Some(2));
        assert_eq!(settings.formatting().use_tabs(), Some(false));
        assert!(settings.formatting().compact_factor_operators());
        assert!(!settings.formatting().break_after_semicolon());
        assert!(settings.expression_type_hints());
    }

    #[test]
    fn rejects_unknown_diagnostic_selectors() {
        let error = ServerSettings::from_value(&serde_json::json!({
            "diagnostics": {
                "disabled": ["not-a-rule"]
            }
        }))
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("unknown diagnostic `not-a-rule`"));
    }

    #[test]
    fn can_disable_every_diagnostic() {
        let settings = ServerSettings::from_value(&serde_json::json!({
            "diagnostics": {
                "enabled": false
            }
        }))
        .unwrap();

        assert!(M2Diagnostic::ALL
            .into_iter()
            .all(|diagnostic| !settings.diagnostics().allows(diagnostic)));
    }

    #[test]
    fn generic_store_replaces_complete_snapshots() {
        let store = SettingsStore::<Vec<u8>>::default();

        store.replace(vec![1, 2, 3]);

        assert_eq!(store.snapshot(), vec![1, 2, 3]);
    }
}
