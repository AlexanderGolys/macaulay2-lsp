//! Small LSP value predicates shared across capabilities.

use tower_lsp::lsp_types::Range as TextRange;
use tower_lsp::lsp_types::*;

/// Whether `position` lies in `range` (start-inclusive, end-exclusive).
pub(crate) fn position_in_range(position: Position, range: TextRange) -> bool {
    if position.line < range.start.line || position.line > range.end.line {
        return false;
    }
    if position.line == range.start.line && position.character < range.start.character {
        return false;
    }
    if position.line == range.end.line && position.character >= range.end.character {
        return false;
    }
    true
}
