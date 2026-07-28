//! Semantic-token classification over inferred and indexed object facts.

use crate::builtin_index::{BuiltinData, InstanceID, Record};
use crate::typesystem::TypeKnowledge;

/// Indexed facts needed specifically for semantic-token classification.
///
/// This is separate from [`TypeKnowledge`] because syntax/type analysis should
/// not depend on editor presentation roles. Both the concrete corpus and a
/// document-scoped package view implement it, so semantic highlighting follows
/// the same import resolution order as the other language features.
pub trait SemanticTokenKnowledge: TypeKnowledge {
    /// Classify an indexed object by name.
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken>;

    /// Classify a local object from its inferred static type.
    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken>;

    /// Whether `name` is a protected object whose class is exactly `Symbol`.
    fn is_protected_symbol(&self, name: &str) -> bool;

    /// Whether indexed facts admit `value_name` for `option_key`.
    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool;
}

/// The LSP-standard token types emitted for M2 syntax and indexed metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum M2SemanticTokenType {
    Type = 0,
    Function = 1,
    Variable = 2,
    Parameter = 3,
    Property = 4,
    Namespace = 5,
    EnumMember = 6,
    Class = 7,
    Keyword = 8,
    String = 9,
    Number = 10,
    Operator = 11,
    Comment = 12,
    Method = 13,
    Regexp = 14,
    Modifier = 15,
}

/// Provenance facts represented as semantic-token modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum M2SemanticTokenProvenance {
    None,
    DefaultLibrary,
    Builtin,
}

/// A semantic-token role plus M2-specific modifier facts for one identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M2SemanticToken {
    pub token_type: M2SemanticTokenType,
    pub is_command: bool,
    pub is_file: bool,
    pub is_manipulator: bool,
    pub is_constructor: bool,
    pub provenance: M2SemanticTokenProvenance,
}

impl BuiltinData {
    /// Classify an indexed object by its runtime class and hierarchy for LSP
    /// semantic tokens.
    pub fn get_semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        semantic_token_from_knowledge(self, name)
    }

    /// Classify a known static type when recoloring a local symbol by inference.
    pub fn get_semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        semantic_token_for_static_type_from_knowledge(self, type_name)
    }

    /// Whether `name` resolves to a protected object whose class is exactly
    /// `Symbol`.
    pub fn is_protected_symbol(&self, name: &str) -> bool {
        let symbol_type = InstanceID::new("Symbol");
        self.get_record(&InstanceID::new(name))
            .is_some_and(|record| record.class == symbol_type && record.protected.unwrap_or(true))
    }

    /// Whether the indexed facts admit `value_name` for any spelling of
    /// `option_key`, ignoring package qualification.
    pub fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        let option_key = option_key
            .rsplit_once('$')
            .map_or(option_key, |(_, name)| name);
        let value_name = value_name
            .rsplit_once('$')
            .map_or(value_name, |(_, name)| name);

        self.option_facts
            .option_values_by_slot
            .iter()
            .filter(|(slot, _)| slot.option.0 == option_key)
            .any(|(_, values)| values.iter().any(|value| value.0 == value_name))
    }
}

impl SemanticTokenKnowledge for BuiltinData {
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        BuiltinData::get_semantic_token(self, name)
    }

    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        BuiltinData::get_semantic_token_for_static_type(self, type_name)
    }

    fn is_protected_symbol(&self, name: &str) -> bool {
        BuiltinData::is_protected_symbol(self, name)
    }

    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        BuiltinData::is_option_value_for_key(self, option_key, value_name)
    }
}

impl<T: SemanticTokenKnowledge + ?Sized> SemanticTokenKnowledge for &T {
    fn semantic_token(&self, name: &str) -> Option<M2SemanticToken> {
        T::semantic_token(self, name)
    }

    fn semantic_token_for_static_type(&self, type_name: &str) -> Option<M2SemanticToken> {
        T::semantic_token_for_static_type(self, type_name)
    }

    fn is_protected_symbol(&self, name: &str) -> bool {
        T::is_protected_symbol(self, name)
    }

    fn is_option_value_for_key(&self, option_key: &str, value_name: &str) -> bool {
        T::is_option_value_for_key(self, option_key, value_name)
    }
}

/// Classify one indexed object using its record and the known type lattice.
pub fn semantic_token_from_knowledge(
    knowledge: &(impl TypeKnowledge + ?Sized),
    name: &str,
) -> Option<M2SemanticToken> {
    let record = knowledge.get_record(&InstanceID::new(name))?;
    let data_type = &record.class;

    let is_command = knowledge.is_subtype(data_type.as_ref(), "Command");
    let is_file = knowledge.is_subtype(data_type.as_ref(), "File");
    let is_manipulator = knowledge.is_subtype(data_type.as_ref(), "Manipulator");
    let is_scripted_functor = knowledge.is_subtype(data_type.as_ref(), "ScriptedFunctor");
    let is_compiled_function = knowledge.is_subtype(data_type.as_ref(), "CompiledFunction")
        || knowledge.is_subtype(data_type.as_ref(), "CompiledFunctionClosure");
    let is_constructor =
        indexed_name_is_constructor(knowledge, &record.name.0) && !is_manipulator && !is_command;
    let provenance = if is_compiled_function {
        M2SemanticTokenProvenance::Builtin
    } else if record.package.as_deref() == Some("Core") {
        M2SemanticTokenProvenance::DefaultLibrary
    } else {
        M2SemanticTokenProvenance::None
    };

    if record_is_type_like(record) {
        return Some(M2SemanticToken {
            token_type: if data_type.as_ref() == "Type" {
                M2SemanticTokenType::Class
            } else {
                M2SemanticTokenType::Type
            },
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
            provenance,
        });
    }

    if knowledge.is_subtype(data_type.as_ref(), "Function")
        || is_scripted_functor
        || is_manipulator
        || is_command
    {
        let has_installed_methods = record
            .callable()
            .is_some_and(|info| !info.methods.is_empty());
        let token_type = if is_manipulator {
            M2SemanticTokenType::Operator
        } else if provenance == M2SemanticTokenProvenance::DefaultLibrary {
            M2SemanticTokenType::Method
        } else if is_command || is_scripted_functor || is_compiled_function {
            M2SemanticTokenType::Function
        } else if has_installed_methods {
            M2SemanticTokenType::Method
        } else {
            M2SemanticTokenType::Function
        };

        Some(M2SemanticToken {
            token_type,
            is_command,
            is_file: false,
            is_manipulator,
            is_constructor,
            provenance,
        })
    } else if knowledge.is_subtype(data_type.as_ref(), "Package") {
        Some(M2SemanticToken {
            token_type: M2SemanticTokenType::Namespace,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
            provenance,
        })
    } else if (knowledge.is_subtype(data_type.as_ref(), "Symbol") || is_file)
        && !knowledge.is_subtype(data_type.as_ref(), "Keyword")
        && !knowledge.is_subtype(data_type.as_ref(), "Operator")
    {
        let is_symbol_class = data_type.as_ref() == "Symbol";
        let token_type = if is_symbol_class && record.protected.unwrap_or(true) {
            M2SemanticTokenType::EnumMember
        } else {
            M2SemanticTokenType::Variable
        };

        Some(M2SemanticToken {
            token_type,
            is_command: false,
            is_file,
            is_manipulator: false,
            is_constructor: false,
            provenance,
        })
    } else {
        Some(M2SemanticToken {
            token_type: M2SemanticTokenType::Variable,
            is_command: false,
            is_file: false,
            is_manipulator: false,
            is_constructor: false,
            provenance,
        })
    }
}

/// Classify a source object from its inferred static type.
pub fn semantic_token_for_static_type_from_knowledge(
    knowledge: &(impl TypeKnowledge + ?Sized),
    type_name: &str,
) -> Option<M2SemanticToken> {
    let is_command = knowledge.is_subtype(type_name, "Command");
    let is_file = knowledge.is_subtype(type_name, "File");
    let is_manipulator = knowledge.is_subtype(type_name, "Manipulator");
    let is_type_valued = knowledge.is_subtype(type_name, "Type");

    let token_type = if type_name.starts_with("MethodFunction") {
        M2SemanticTokenType::Method
    } else if knowledge.is_subtype(type_name, "Package") {
        M2SemanticTokenType::Namespace
    } else if is_type_valued {
        if knowledge
            .get_record(&InstanceID::new(type_name))
            .is_some_and(|record| record.class.as_ref() == "Type")
        {
            M2SemanticTokenType::Class
        } else {
            M2SemanticTokenType::Type
        }
    } else if knowledge.is_subtype(type_name, "Function")
        || knowledge.is_subtype(type_name, "ScriptedFunctor")
        || is_manipulator
        || is_command
    {
        if is_command {
            M2SemanticTokenType::Function
        } else if is_manipulator {
            M2SemanticTokenType::Operator
        } else {
            M2SemanticTokenType::Function
        }
    } else if is_file {
        M2SemanticTokenType::Variable
    } else if knowledge.is_subtype(type_name, "Symbol") {
        M2SemanticTokenType::EnumMember
    } else {
        return None;
    };

    Some(M2SemanticToken {
        token_type,
        is_command,
        is_file,
        is_manipulator,
        is_constructor: false,
        provenance: M2SemanticTokenProvenance::None,
    })
}

fn indexed_name_is_constructor(knowledge: &(impl TypeKnowledge + ?Sized), name: &str) -> bool {
    let unqualified_name = name.rsplit_once('$').map_or(name, |(_, name)| name);
    let Some(target_name) = unqualified_name.strip_prefix("to") else {
        return false;
    };
    if target_name.is_empty() {
        return false;
    }

    knowledge
        .get_record(&InstanceID::new(target_name))
        .is_some_and(record_is_type_like)
}

fn record_is_type_like(record: &Record) -> bool {
    record
        .type_info()
        .is_some_and(|type_info| type_info.parent_type.is_some())
}
