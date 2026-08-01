//! Static type inference over the canonical object facts.
//!
//! This module does not evaluate Macaulay2. It queries the active object
//! environment only to answer conservative type and dispatch questions.

use crate::builtin_index::{CallableInfo, MethodSignature};
use tower_lsp::lsp_types::Position;

use crate::object_registry::{
    ObjectId, ObjectKnowledge, ObjectName, ObjectRegistry, ObjectRegistryView, TypeId,
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
pub trait TypeKnowledge: ObjectKnowledge {
    /// Whether external type facts are available for diagnostics.
    fn is_available(&self) -> bool {
        true
    }

    /// Whether a later package inclusion shadows a global source definition of
    /// `name` made at `source_position`.
    fn shadows_source(&self, _name: &ObjectName, _source_position: Position) -> bool {
        false
    }

    /// Resolve two nominal references and compare their validated type IDs.
    fn is_subtype(&self, child: &ObjectName, parent: &ObjectName) -> bool {
        self.resolve_type_id(child)
            .zip(self.resolve_type_id(parent))
            .is_some_and(|(child, parent)| self.is_subtype_id(&child, &parent))
    }

    fn has_type_role(&self, candidate: &ObjectName, role: TypeRole) -> bool {
        self.resolve_type_id(candidate)
            .zip(self.resolve_type_id(&role.object_name()))
            .is_some_and(|(candidate, role)| self.is_subtype_id(&candidate, &role))
    }

    fn type_role_id(&self, role: TypeRole) -> Option<TypeId> {
        self.resolve_type_id(&role.object_name())
    }

    /// Whether `smaller` is a strict componentwise subtype of `bigger`.
    fn domain_strictly_smaller(&self, smaller: &[ObjectId], bigger: &[ObjectId]) -> bool {
        if smaller == bigger || smaller.len() != bigger.len() {
            return false;
        }

        let mut strict = false;
        for (small, big) in smaller.iter().zip(bigger) {
            if small == big {
                continue;
            }
            if self.dispatch_matches(small, big) {
                strict = true;
                continue;
            }
            return false;
        }
        strict
    }

    /// Resolve a call to exactly one known return type, or stay silent when the
    /// available metadata is ambiguous or incomplete.
    fn resolve_call_return_type_with_options(
        &self,
        callable: &ObjectName,
        argument_types: &[Option<ObjectId>],
        _options: &[LiteralOption],
    ) -> Option<TypeId> {
        self.resolve_installed_method_codomain(callable, argument_types)
            .or_else(|| self.get_record(callable)?.callable()?.typical_value.clone())
    }

    /// Resolve a call only when its known argument types select one installed
    /// method codomain.
    fn resolve_installed_method_codomain(
        &self,
        callable: &ObjectName,
        argument_types: &[Option<ObjectId>],
    ) -> Option<TypeId> {
        let callable_info = self.get_record(callable)?.callable()?;
        let mut specialized_candidates = Vec::new();
        let mut general_candidates = Vec::new();
        for method in &callable_info.methods {
            if method.domain.is_empty() || method.domain.len() != argument_types.len() {
                continue;
            }
            if !self.domain_matches(&method.domain, argument_types) {
                continue;
            }
            let Some((codomain, is_specialized)) = callable_info.effective_codomain(method) else {
                continue;
            };
            let candidate = (method.domain.as_slice(), codomain);
            if is_specialized {
                specialized_candidates.push(candidate);
            } else {
                general_candidates.push(candidate);
            }
        }

        let mut candidates = if specialized_candidates.is_empty() {
            general_candidates
        } else {
            specialized_candidates
        };
        let originals = candidates.clone();
        candidates.retain(|candidate| {
            !originals
                .iter()
                .any(|other| self.domain_strictly_smaller(other.0, candidate.0))
        });
        candidates.sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
        candidates.dedup();
        match candidates.as_slice() {
            [(_, codomain)] => Some((*codomain).clone()),
            _ => None,
        }
    }

    /// Whether every known argument can inhabit the corresponding domain slot.
    /// Unknown arguments remain possible for editor-facing signature filtering.
    fn domain_possibly_matches(
        &self,
        domain: &[ObjectId],
        argument_types: &[Option<ObjectId>],
    ) -> bool {
        domain.len() == argument_types.len()
            && domain.iter().zip(argument_types).all(|(expected, actual)| {
                actual
                    .as_ref()
                    .is_none_or(|actual| self.dispatch_matches(actual, expected))
            })
    }

    /// Whether every argument is known and inhabits the corresponding slot.
    fn domain_matches(&self, domain: &[ObjectId], argument_types: &[Option<ObjectId>]) -> bool {
        argument_types.iter().all(Option::is_some)
            && self.domain_possibly_matches(domain, argument_types)
    }

    /// Whether one runtime dispatch identity satisfies a method-domain slot.
    /// Singleton domains match by exact identity; type domains additionally
    /// admit descendants in the registered partial order.
    fn dispatch_matches(&self, actual: &ObjectId, expected: &ObjectId) -> bool {
        actual == expected
            || self
                .type_id(actual)
                .zip(self.type_id(expected))
                .is_some_and(|(actual, expected)| self.is_subtype_id(&actual, &expected))
    }
}

/// Finite antichain presentation of the free cocompletion of a partial order.
///
/// Each stored generator represents its principal up-set. Their union is kept
/// minimal by removing generators subsumed by another generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredType {
    minimal_generators: Vec<ObjectName>,
}

impl InferredType {
    /// Construct one principal up-set.
    fn principal_upset(generator: ObjectName) -> Self {
        Self {
            minimal_generators: vec![generator],
        }
    }

    /// Return the generator when this cocompletion value is principal.
    pub fn principal(&self) -> Option<&ObjectName> {
        match self.minimal_generators.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// Form a union and minimize it using the supplied partial-order relation.
    fn union_by(self, other: Self, is_below: impl Fn(&ObjectName, &ObjectName) -> bool) -> Self {
        let mut minimal_generators = self.minimal_generators;
        for generator in other.minimal_generators {
            if !minimal_generators.contains(&generator) {
                minimal_generators.push(generator);
            }
        }
        let candidates = minimal_generators.clone();
        minimal_generators.retain(|generator| {
            !candidates
                .iter()
                .any(|other| other != generator && is_below(generator, other))
        });
        Self { minimal_generators }
    }

    /// The whole known type order, represented by its `Thing` generator.
    pub fn unknown() -> Self {
        Self::of("Thing")
    }

    /// Construct the principal up-set generated by a named type.
    pub fn of(name: &str) -> Self {
        Self::from_id(ObjectName::new(name))
    }

    /// Construct the principal up-set generated by a linked type reference.
    pub fn from_id(id: ObjectName) -> Self {
        Self::principal_upset(id)
    }

    /// Human-readable antichain label.
    pub fn label(&self) -> Option<String> {
        (!self.minimal_generators.is_empty()).then(|| {
            self.minimal_generators
                .iter()
                .map(ObjectName::name)
                .collect::<Vec<_>>()
                .join(" | ")
        })
    }

    /// Union two inferred type values using the registry's type partial order.
    pub fn join(self, other: Self, knowledge: &(impl TypeKnowledge + ?Sized)) -> Self {
        self.union_by(other, |child, parent| knowledge.is_subtype(child, parent))
    }
}

/// Supplies the object environment effective at one source position.
pub trait PositionedTypeKnowledge {
    type Knowledge<'a>: TypeKnowledge
    where
        Self: 'a;

    fn at_position(&self, position: Position) -> Self::Knowledge<'_>;
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

    fn at_position(&self, _position: Position) -> Self::Knowledge<'_> {
        *self
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
    fn dispatch_walks_the_lattice_to_the_installed_supertype_method() {
        // dim is installed on Ring; PolynomialRing <: Ring.
        let corpus = concat!(
            r#"{"kind":"type","name":"Thing","aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"type","name":"Ring","parent":"Thing","ancestors":["Thing"],"subtypes":["PolynomialRing"],"aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"type","name":"PolynomialRing","parent":"Ring","ancestors":["Ring","Thing"],"subtypes":[],"aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"type","name":"ZZ","parent":"Thing","aliases":[],"extra_keys":[]}"#,
            "\n",
            r#"{"kind":"function","name":"dim","methods":[{"domain":["$Core$Ring"],"typicalValue":"$Core$ZZ"}],"aliases":[],"extra_keys":[]}"#,
        );
        let builtins = ObjectRegistry::load(corpus);
        let type_id = |name: &str| {
            builtins
                .resolve_type_id(&ObjectName::new(name))
                .expect("test type should resolve")
        };

        // Exact match.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                &ObjectName::new("dim"),
                &[Some(type_id("Ring").object().clone())],
                &[],
            ),
            Some(type_id("ZZ"))
        );
        // Subtype dispatch: PolynomialRing has no own method, walks up to Ring's.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                &ObjectName::new("dim"),
                &[Some(type_id("PolynomialRing").object().clone())],
                &[],
            ),
            Some(type_id("ZZ"))
        );
        // No applicable method (Thing is a supertype, not a subtype) ⇒ silent.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                &ObjectName::new("dim"),
                &[Some(type_id("Thing").object().clone())],
                &[],
            ),
            None
        );
    }

    #[test]
    fn dispatch_domains_can_name_singleton_objects_as_well_as_types() {
        let corpus = concat!(
            r#"{"kind":"type","name":"Thing"}"#,
            "\n",
            r#"{"kind":"type","name":"ToricDivisor","parent":"Thing"}"#,
            "\n",
            r#"{"kind":"type","name":"SheafOfRings","parent":"Thing"}"#,
            "\n",
            r#"{"kind":"function","name":"OO"}"#,
            "\n",
            r#"{"kind":"function","name":"apply","methods":[{"domain":["OO","ToricDivisor"],"typicalValue":"SheafOfRings"}]}"#,
        );
        let knowledge = ObjectRegistry::load(corpus);
        let oo = knowledge
            .resolve_object(&ObjectName::new("OO"))
            .expect("singleton dispatch object should resolve");
        let divisor = knowledge
            .resolve_object(&ObjectName::new("ToricDivisor"))
            .expect("type dispatch object should resolve");
        let sheaf = knowledge
            .resolve_type_id(&ObjectName::new("SheafOfRings"))
            .expect("codomain type should resolve");

        assert_eq!(
            knowledge.resolve_call_return_type_with_options(
                &ObjectName::new("apply"),
                &[Some(oo), Some(divisor)],
                &[],
            ),
            Some(sheaf),
        );
    }

    #[test]
    fn resolves_real_gb_codomain_from_the_type_index() {
        let builtins = ObjectRegistry::load(include_str!("./data/m2-index.jsonl"));
        let type_id = |name: &str| {
            builtins
                .resolve_type_id(&ObjectName::new(name))
                .expect("test type should resolve")
        };
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                &ObjectName::new("gb"),
                &[Some(type_id("Ideal").object().clone())],
                &[],
            ),
            Some(type_id("GroebnerBasis"))
        );
    }

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
