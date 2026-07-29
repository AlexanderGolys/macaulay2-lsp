//! Test-only semantic fixtures shared by crate tests.

use crate::analysis::Analysis;
use crate::builtin_index::Record;
use crate::node_metadata::M2Node;
use crate::object_registry::{ObjectId, ObjectKnowledge, ObjectName, Type, TypeId, TypeStore};
use crate::source::DocumentSource;
use crate::typesystem::{LiteralOption, PositionedTypeKnowledge, TypeKnowledge};
use tower_lsp::lsp_types::Position;

/// Empty semantic knowledge for parser- and scope-only tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTypeKnowledge;

impl ObjectKnowledge for NoTypeKnowledge {
    fn object(&self, _object_id: &ObjectId) -> Option<&Record> {
        None
    }

    fn resolve_object(&self, _name: &ObjectName) -> Option<ObjectId> {
        None
    }

    fn resolve_type(&self, _name: &ObjectName) -> Option<Type<'_>> {
        None
    }
}

impl TypeStore for NoTypeKnowledge {
    fn parent_type_id(&self, _type_id: &TypeId) -> Option<TypeId> {
        None
    }
}

impl TypeKnowledge for NoTypeKnowledge {
    fn is_available(&self) -> bool {
        false
    }

    fn resolve_call_return_type_with_options(
        &self,
        _callable: &ObjectName,
        _argument_types: &[Option<ObjectName>],
        _options: &[LiteralOption],
    ) -> Option<ObjectName> {
        None
    }
}

impl PositionedTypeKnowledge for NoTypeKnowledge {
    type Knowledge<'a> = Self;

    fn at_position(&self, _position: Position) -> Self::Knowledge<'_> {
        *self
    }
}

/// Analyze one syntax fixture without external object knowledge.
pub fn analyze(root: M2Node<'_>) -> Analysis {
    let source = DocumentSource::new(root.text().to_string());
    Analysis::new_with_knowledge(root, &source, &NoTypeKnowledge)
}
