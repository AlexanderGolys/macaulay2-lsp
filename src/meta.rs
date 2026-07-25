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

#[derive(Debug, Clone, Copy, Default)]
pub struct Meta<'a> {
    pub symbol_kind: Option<SymbolKind>,
    pub binding_role: Option<BindingRole>,
    pub type_name: Option<&'a str>,
}

pub trait Metadata {
    fn meta(&self) -> Meta<'_>;
}

impl Metadata for Meta<'_> {
    fn meta(&self) -> Meta<'_> {
        *self
    }
}
