//! Static type inference over the canonical object facts.
//!
//! This module does not evaluate Macaulay2. It queries generated corpus facts
//! only to answer the conservative type and dispatch questions used by
//! [`crate::analysis`].

use crate::builtin_index::{BuiltinData, CallableInfo, InstanceID, MethodSignature, Record};
use crate::object_registry::ObjectKnowledge;

/// The semantic facts analysis needs from an external object registry.
///
/// Keeping this as a narrow trait decouples the analysis engine from the
/// concrete generated-corpus store. The normal server implements it with
/// [`BuiltinData`]; syntax-only tests use an empty implementation. A future
/// workspace/package view can implement the same contract without teaching
/// analysis about its storage or resolution order.
pub trait TypeKnowledge: ObjectKnowledge {
    fn is_available(&self) -> bool;

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<InstanceID>],
        options: &[(String, String)],
    ) -> Option<InstanceID>;

    fn is_subtype(&self, child: &str, parent: &str) -> bool;
}

/// Supplies the semantic index for one document's imported-package set.
///
/// The associated type is a borrowing view: providers may return themselves,
/// or construct a lightweight scoped view without merging/copying indexes.
pub trait TypeKnowledgeProvider {
    type Knowledge<'a>: TypeKnowledge
    where
        Self: 'a;

    fn knowledge_for<'a>(&'a self, imported_packages: &[String]) -> Self::Knowledge<'a>;
}

/// Empty semantic knowledge for parser/scope-only analysis.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTypeKnowledge;

impl BuiltinData {
    /// Resolve a call to exactly one known return type, or stay silent when the
    /// available metadata is ambiguous or incomplete. Literal option facts are
    /// accepted for the option-sensitive dispatch path.
    pub fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<InstanceID>],
        _literal_options: &[(String, String)],
    ) -> Option<InstanceID> {
        if let Some(codomain) = self.resolve_installed_method_codomain(callable, argument_types) {
            return Some(codomain);
        }

        self.get_record(&InstanceID::new(callable))?
            .callable()?
            .typical_value
            .clone()
    }

    /// Resolve a call only when its known argument types select one installed
    /// method codomain. An explicit method codomain is preferred to a callable
    /// typical value.
    fn resolve_installed_method_codomain(
        &self,
        callable: &str,
        argument_types: &[Option<InstanceID>],
    ) -> Option<InstanceID> {
        let record = self.get_record(&InstanceID::new(callable))?;
        let callable_info = record.callable()?;
        let mut specialized_candidates = Vec::new();
        let mut general_candidates = Vec::new();
        for method in &callable_info.methods {
            if method.domain.is_empty() {
                continue;
            }
            if method.domain.len() != argument_types.len() {
                continue;
            }
            if !argument_types
                .iter()
                .zip(&method.domain)
                .all(|(argument_type, domain_type)| match argument_type {
                    Some(argument_type) => {
                        argument_type == domain_type || self.is_subtype(argument_type, domain_type)
                    }
                    None => false,
                })
            {
                continue;
            }
            let Some((codomain, is_specialized)) = effective_method_codomain(callable_info, method)
            else {
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
        take_nonminimal_signatures(self, &mut candidates);
        candidates.sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
        candidates.dedup();
        if let [(_, codomain)] = candidates.as_slice() {
            Some((*codomain).clone())
        } else {
            None
        }
    }

    /// Check if `child` is a subtype of `parent`, inclusively.
    pub fn is_subtype(&self, child: impl AsRef<str>, parent: impl AsRef<str>) -> bool {
        let child = child.as_ref();
        let parent = parent.as_ref();
        child == parent
            || self
                .get_record(&InstanceID::new(child))
                .and_then(Record::type_info)
                .is_some_and(|type_info| {
                    type_info
                        .ancestors
                        .binary_search_by(|candidate| candidate.as_ref().cmp(parent))
                        .is_ok()
                })
    }
}

impl TypeKnowledge for BuiltinData {
    fn is_available(&self) -> bool {
        true
    }

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<InstanceID>],
        options: &[(String, String)],
    ) -> Option<InstanceID> {
        BuiltinData::resolve_call_return_type_with_options(self, callable, argument_types, options)
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        BuiltinData::is_subtype(self, child, parent)
    }
}

impl<T: TypeKnowledge + ?Sized> TypeKnowledge for &T {
    fn is_available(&self) -> bool {
        T::is_available(self)
    }

    fn resolve_call_return_type_with_options(
        &self,
        callable: &str,
        argument_types: &[Option<InstanceID>],
        options: &[(String, String)],
    ) -> Option<InstanceID> {
        T::resolve_call_return_type_with_options(self, callable, argument_types, options)
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        T::is_subtype(self, child, parent)
    }
}

impl TypeKnowledgeProvider for BuiltinData {
    type Knowledge<'a> = &'a BuiltinData;

    fn knowledge_for<'a>(&'a self, _imported_packages: &[String]) -> Self::Knowledge<'a> {
        self
    }
}

#[cfg(test)]
impl TypeKnowledge for NoTypeKnowledge {
    fn is_available(&self) -> bool {
        false
    }

    fn resolve_call_return_type_with_options(
        &self,
        _callable: &str,
        _argument_types: &[Option<InstanceID>],
        _options: &[(String, String)],
    ) -> Option<InstanceID> {
        None
    }

    fn is_subtype(&self, _child: &str, _parent: &str) -> bool {
        false
    }
}

fn take_nonminimal_signatures(
    knowledge: &(impl TypeKnowledge + ?Sized),
    signatures: &mut Vec<(&[InstanceID], &InstanceID)>,
) {
    let originals = signatures.clone();
    signatures.retain(|candidate| {
        !originals
            .iter()
            .any(|other| domain_strictly_smaller(knowledge, other.0, candidate.0))
    });
}

/// Resolve the effective codomain of an installed method and whether it came
/// from that method rather than the callable's general typical value.
pub(crate) fn effective_method_codomain<'a>(
    callable: &'a CallableInfo,
    method: &'a MethodSignature,
) -> Option<(&'a InstanceID, bool)> {
    method
        .codomain
        .as_ref()
        .map(|codomain| (codomain, true))
        .or_else(|| {
            callable
                .typical_value
                .as_ref()
                .map(|codomain| (codomain, false))
        })
}

/// Whether `smaller` is a strict componentwise subtype of `bigger`.
pub(crate) fn domain_strictly_smaller(
    knowledge: &(impl TypeKnowledge + ?Sized),
    smaller: &[InstanceID],
    bigger: &[InstanceID],
) -> bool {
    if smaller == bigger {
        return false;
    }

    if smaller.len() != bigger.len() {
        return false;
    }

    let mut strict = false;
    for (small, big) in smaller.iter().zip(bigger) {
        if small == big {
            continue;
        }
        if knowledge.is_subtype(small.as_ref(), big.as_ref()) {
            strict = true;
            continue;
        }
        return false;
    }

    strict
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
            r#"{"kind":"function","name":"dim","methods":[{"domain":["Ring"],"typicalValue":"ZZ"}],"aliases":[],"extra_keys":[]}"#,
        );
        let builtins = BuiltinData::load_from_index(corpus);

        // Exact match.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                "dim",
                &[Some(InstanceID::new("Ring"))],
                &[],
            ),
            Some(InstanceID::new("ZZ"))
        );
        // Subtype dispatch: PolynomialRing has no own method, walks up to Ring's.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                "dim",
                &[Some(InstanceID::new("PolynomialRing"))],
                &[],
            ),
            Some(InstanceID::new("ZZ"))
        );
        // No applicable method (Thing is a supertype, not a subtype) ⇒ silent.
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                "dim",
                &[Some(InstanceID::new("Thing"))],
                &[],
            ),
            None
        );
    }

    #[test]
    fn resolves_real_gb_codomain_from_the_type_index() {
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));
        assert_eq!(
            builtins.resolve_call_return_type_with_options(
                "gb",
                &[Some(InstanceID::new("Ideal"))],
                &[],
            ),
            Some(InstanceID::new("GroebnerBasis"))
        );
    }

    #[test]
    fn is_subtype_holds_for_the_immediate_parent_edge() {
        // Regression: the corpus `ancestors` field omits the immediate parent, so
        // a lattice built from it alone made `is_subtype(child, parent)` succeed
        // only reflexively (`new Type` worked, `new SelfInitializingType` did not).
        let builtins = BuiltinData::load_from_index(include_str!("./data/m2-index.jsonl"));

        // Direct parent edges (verified against M2): SelfInitializingType <: Type,
        // Array <: VisibleList.
        assert!(builtins.is_subtype("SelfInitializingType", "Type"));
        assert!(builtins.is_subtype(InstanceID::new("Array"), InstanceID::new("VisibleList")));
        // Transitive edges still hold, and unrelated types stay unrelated.
        assert!(builtins.is_subtype("Array", "Thing"));
        assert!(!builtins.is_subtype("Array", "Type"));
    }
}
