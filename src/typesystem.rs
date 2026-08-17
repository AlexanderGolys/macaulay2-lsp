//! Static type inference over the canonical object facts.
//!
//! This module does not evaluate Macaulay2. It queries the active object
//! environment only to answer conservative type and dispatch questions.

use crate::builtin_index::{CallableInfo, MethodSignature};
use tower_lsp::lsp_types::Position;

use crate::object_registry::{
    ObjectKnowledge, ObjectName, ObjectRegistry, ObjectRegistryView, TypeId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeRole {
    Boolean,
    Function,
    MethodFunction,
    Package,
    Ring,
    String,
    Thing,
    Type,
    VisibleList,
}

/// Evidence available for one nominal subtype question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtypeEvidence {
    Proven,
    Disproven,
    Unknown,
}

impl TypeRole {
    pub fn object_name(self) -> ObjectName {
        ObjectName::new(match self {
            Self::Boolean => "Boolean",
            Self::Function => "Function",
            Self::MethodFunction => "MethodFunction",
            Self::Package => "Package",
            Self::Ring => "Ring",
            Self::String => "String",
            Self::Thing => "Thing",
            Self::Type => "Type",
            Self::VisibleList => "VisibleList",
        })
    }
}

/// One statically known option key and literal value at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralOption {
    pub option: ObjectName,
    pub value: ObjectName,
}

/// The semantic facts analysis needs from an external object registry.
///
/// Keeping this as a narrow trait decouples the analysis engine from registry
/// storage and resolution order. The complete [`ObjectRegistry`], an ordered
/// package view, and syntax-only test knowledge all implement the same contract.
pub trait TypeKnowledge: ObjectKnowledge + PositionedTypeKnowledge {
    /// Whether external type facts are available for diagnostics.
    fn is_available(&self) -> bool {
        true
    }

    /// Whether a later package inclusion shadows a global source definition of
    /// `name` made at `source_position`.
    fn shadows_source(&self, _name: &ObjectName, _source_position: Position) -> bool {
        false
    }

    fn subtype_evidence(&self, child: &ObjectName, parent: &ObjectName) -> SubtypeEvidence {
        let Some((child, parent)) = self
            .resolve_type_id(child)
            .zip(self.resolve_type_id(parent))
        else {
            return SubtypeEvidence::Unknown;
        };
        if self.is_subtype_id(&child, &parent) {
            SubtypeEvidence::Proven
        } else {
            SubtypeEvidence::Disproven
        }
    }

    fn is_subtype(&self, child: &ObjectName, parent: &ObjectName) -> bool {
        self.subtype_evidence(child, parent) == SubtypeEvidence::Proven
    }

    fn has_type_role(&self, candidate: &ObjectName, role: TypeRole) -> bool {
        self.type_role_evidence(candidate, role) == SubtypeEvidence::Proven
    }

    fn type_role_evidence(&self, candidate: &ObjectName, role: TypeRole) -> SubtypeEvidence {
        self.subtype_evidence(candidate, &role.object_name())
    }

    fn type_role_id(&self, role: TypeRole) -> Option<TypeId> {
        self.resolve_type_id(&role.object_name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredType {
    exact_points: Vec<ObjectName>,
    upward_generators: Vec<ObjectName>,
}

impl InferredType {
    pub fn diverges() -> Self {
        Self {
            exact_points: Vec::new(),
            upward_generators: Vec::new(),
        }
    }

    pub fn exact(name: &str) -> Self {
        Self::exact_from_id(ObjectName::new(name))
    }

    pub fn exact_from_id(point: ObjectName) -> Self {
        Self {
            exact_points: vec![point],
            upward_generators: Vec::new(),
        }
    }

    pub fn upward(name: &str) -> Self {
        Self::upward_from_id(ObjectName::new(name))
    }

    pub fn upward_from_id(generator: ObjectName) -> Self {
        Self {
            exact_points: Vec::new(),
            upward_generators: vec![generator],
        }
    }

    pub fn single(&self) -> Option<&ObjectName> {
        match (
            self.exact_points.as_slice(),
            self.upward_generators.as_slice(),
        ) {
            ([only], []) | ([], [only]) => Some(only),
            _ => None,
        }
    }

    pub fn exact_points(&self) -> impl Iterator<Item = &ObjectName> {
        self.exact_points.iter()
    }

    pub fn upward_generators(&self) -> impl Iterator<Item = &ObjectName> {
        self.upward_generators.iter()
    }

    pub fn unknown() -> Self {
        Self::upward("Thing")
    }

    pub fn label(&self) -> Option<String> {
        (!self.exact_points.is_empty() || !self.upward_generators.is_empty()).then(|| {
            self.exact_points
                .iter()
                .chain(&self.upward_generators)
                .map(ObjectName::name)
                .collect::<Vec<_>>()
                .join(" | ")
        })
    }

    pub fn subset_label(
        &self,
        has_strict_member_above: impl Fn(&ObjectName) -> bool,
    ) -> Option<String> {
        (!self.exact_points.is_empty() || !self.upward_generators.is_empty()).then(|| {
            self.upward_generators
                .iter()
                .map(|generator| {
                    if has_strict_member_above(generator) {
                        format!("↑{}", generator.name())
                    } else {
                        generator.name().to_string()
                    }
                })
                .chain(
                    self.exact_points
                        .iter()
                        .map(|point| point.name().to_string()),
                )
                .collect::<Vec<_>>()
                .join(" | ")
        })
    }

    pub fn possibility_by(
        &self,
        candidate: &ObjectName,
        evidence: impl Fn(&ObjectName, &ObjectName) -> SubtypeEvidence,
    ) -> SubtypeEvidence {
        let mut result = SubtypeEvidence::Disproven;
        if self.exact_points.iter().any(|point| point == candidate) {
            return SubtypeEvidence::Proven;
        }
        for generator in &self.upward_generators {
            match evidence(candidate, generator) {
                SubtypeEvidence::Proven => return SubtypeEvidence::Proven,
                SubtypeEvidence::Unknown => result = SubtypeEvidence::Unknown,
                SubtypeEvidence::Disproven => {}
            }
        }
        result
    }

    pub fn join_by(self, other: Self, is_below: impl Fn(&ObjectName, &ObjectName) -> bool) -> Self {
        let mut exact_points = self.exact_points;
        for point in other.exact_points {
            if !exact_points.contains(&point) {
                exact_points.push(point);
            }
        }

        let mut upward_generators = self.upward_generators;
        for generator in other.upward_generators {
            if !upward_generators.contains(&generator) {
                upward_generators.push(generator);
            }
        }

        let candidates = upward_generators.clone();
        upward_generators.retain(|generator| {
            !candidates
                .iter()
                .any(|other| other != generator && is_below(generator, other))
        });
        exact_points.retain(|point| {
            !upward_generators
                .iter()
                .any(|generator| is_below(point, generator))
        });

        Self {
            exact_points,
            upward_generators,
        }
    }
}

/// Supplies the object environment effective at one source position.
pub trait PositionedTypeKnowledge {
    type Knowledge<'a>: TypeKnowledge + PositionedTypeKnowledge
    where
        Self: 'a;

    fn at_position(&self, position: Position) -> Self::Knowledge<'_>;
}

impl<T: PositionedTypeKnowledge + ?Sized> PositionedTypeKnowledge for &T {
    type Knowledge<'a>
        = T::Knowledge<'a>
    where
        Self: 'a;

    fn at_position(&self, position: Position) -> Self::Knowledge<'_> {
        T::at_position(self, position)
    }
}

impl PositionedTypeKnowledge for ObjectRegistry {
    type Knowledge<'a> = ObjectRegistryView<'a>;

    fn at_position(&self, position: Position) -> Self::Knowledge<'_> {
        self.at(position)
    }
}

impl PositionedTypeKnowledge for ObjectRegistryView<'_> {
    type Knowledge<'a>
        = Self
    where
        Self: 'a;

    fn at_position(&self, position: Position) -> Self::Knowledge<'_> {
        self.at(position)
    }
}

impl TypeKnowledge for ObjectRegistry {}

impl TypeKnowledge for ObjectRegistryView<'_> {
    fn shadows_source(&self, name: &ObjectName, source_position: Position) -> bool {
        ObjectRegistryView::shadows_source(self, name, source_position)
    }
}

impl<T: TypeKnowledge + ?Sized> TypeKnowledge for &T {
    fn is_available(&self) -> bool {
        T::is_available(self)
    }

    fn shadows_source(&self, name: &ObjectName, source_position: Position) -> bool {
        T::shadows_source(self, name, source_position)
    }
}

impl CallableInfo {
    /// Resolve a method's effective codomain and whether it is specialized.
    pub fn effective_codomain<'a>(
        &'a self,
        method: &'a MethodSignature,
    ) -> Option<(&'a TypeId, bool)> {
        method
            .codomain
            .as_ref()
            .map(|codomain| (codomain, true))
            .or_else(|| {
                self.typical_value
                    .as_ref()
                    .map(|codomain| (codomain, false))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_subtype_holds_for_the_immediate_parent_edge() {
        // Regression: the corpus `ancestors` field omits the immediate parent, so
        // a lattice built from it alone made `is_subtype(child, parent)` succeed
        // only reflexively (`new Type` worked, `new SelfInitializingType` did not).
        let builtins = ObjectRegistry::load(include_str!("./data/m2-index.jsonl"));

        // Direct parent edges (verified against M2): SelfInitializingType <: Type,
        // Array <: VisibleList.
        let subtype = |child: &str, parent: &str| {
            builtins.is_subtype(&ObjectName::new(child), &ObjectName::new(parent))
        };
        assert!(subtype("SelfInitializingType", "Type"));
        assert!(subtype("Array", "VisibleList"));
        // Transitive edges still hold, and unrelated types stay unrelated.
        assert!(subtype("Array", "Thing"));
        assert!(!subtype("Array", "Type"));
    }
}
