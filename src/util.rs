//! Small LSP value predicates shared across capabilities.

use tower_lsp::lsp_types::Range as TextRange;
use tower_lsp::lsp_types::*;

#[macro_export]
macro_rules! pos {
    () => {
        tower_lsp::lsp_types::Position::new(0, 0)
    };
    ($line:expr, $character:expr) => {
        tower_lsp::lsp_types::Position::new($line, $character)
    };
}

#[macro_export]
macro_rules! pos_max {
    () => {
        tower_lsp::lsp_types::Position::new(u32::MAX, u32::MAX)
    };
}

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

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::Position;

    #[test]
    fn position_macros_construct_requested_sentinels() {
        assert_eq!(pos!(), Position::new(0, 0));
        assert_eq!(pos!(3, 5), Position::new(3, 5));
        assert_eq!(pos_max!(), Position::new(u32::MAX, u32::MAX));
    }
}
