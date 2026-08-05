//! Typed, grammar-local access to Tree-sitter nodes used throughout the server.

mod kind;
mod node;
mod parser;

pub use kind::{NodeKind, NodeKindMetadata};
pub use node::{BinaryExpressionNode, LambdaExpressionNode, M2Node};
pub use parser::{M2Parser, M2Tree};

#[cfg(test)]
mod tests;
