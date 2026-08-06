//! Shared, optional views over semantic graph nodes.
//!
//! Analysis owns the concrete records and their invariants. Features consume
//! these borrowed views instead of requiring a copied record per capability.

use tower_lsp::lsp_types::SymbolKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingRole {
    Ordinary,
    Parameter,
}

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub symbol_kind: Option<SymbolKind>,
    pub binding_role: Option<BindingRole>,
    pub type_label: Option<String>,
}

pub trait Metadata {
    fn meta(&self) -> Meta;
}

impl Metadata for Meta {
    fn meta(&self) -> Meta {
        self.clone()
    }
}
