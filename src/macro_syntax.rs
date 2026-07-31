//! Recognition of the source-level `$name ... $` syntax provided by the
//! ProceduralMacros package.
//!
//! The upstream M2 parser intentionally does not know this extension. We mask
//! only matched sigils with same-width spaces, then let Tree-sitter parse the
//! unchanged names and bodies. Strings and comments are opaque, matching the
//! package's scanner.

use crate::source::ByteRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MacroSyntax {
    masked_text: String,
    name_spans: Vec<ByteRange>,
}

impl MacroSyntax {
    pub(crate) fn scan(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut masked = bytes.to_vec();
        let mut name_spans = Vec::new();
        let mut pending_names = Vec::new();
        let mut pending_sigils = Vec::new();
        let mut stack_depth = 0usize;
        let mut index = 0usize;

        while index < bytes.len() {
            if let Some(end) = opaque_construct_end(bytes, index) {
                index = end;
                continue;
            }

            if opens_macro(bytes, index) {
                let name_end = macro_name_end(bytes, index);
                stack_depth += 1;
                pending_sigils.push(index);
                pending_names.push(index + 1..name_end);
                index = name_end;
                continue;
            }

            if stack_depth > 0 && closes_macro(bytes, index) {
                pending_sigils.push(index);
                stack_depth -= 1;
                index += 1;
                if stack_depth == 0 {
                    for sigil in pending_sigils.drain(..) {
                        masked[sigil] = b' ';
                    }
                    name_spans.append(&mut pending_names);
                }
                continue;
            }

            index += 1;
        }

        Self {
            masked_text: String::from_utf8(masked)
                .expect("masking ASCII sigils preserves valid UTF-8"),
            name_spans,
        }
    }

    pub(crate) fn parse_text(&self) -> &str {
        &self.masked_text
    }

    pub(crate) fn has_macros(&self) -> bool {
        !self.name_spans.is_empty()
    }

    pub(crate) fn is_macro_name(&self, start_byte: usize, end_byte: usize) -> bool {
        self.name_spans
            .iter()
            .any(|span| span.start == start_byte && span.end == end_byte)
    }
}

fn opens_macro(bytes: &[u8], index: usize) -> bool {
    bytes.get(index) == Some(&b'$')
        && (index == 0 || bytes[index - 1].is_ascii_whitespace())
        && bytes.get(index + 1).is_some_and(u8::is_ascii_alphanumeric)
}

fn closes_macro(bytes: &[u8], index: usize) -> bool {
    bytes.get(index) == Some(&b'$')
        && (index == 0 || bytes[index - 1].is_ascii_whitespace())
        && bytes
            .get(index + 1)
            .is_none_or(|next| !next.is_ascii_alphanumeric())
}

fn macro_name_end(bytes: &[u8], opening: usize) -> usize {
    let mut index = opening + 1;
    while bytes.get(index).is_some_and(u8::is_ascii_alphanumeric) {
        index += 1;
    }
    index
}

fn opaque_construct_end(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) == Some(&b'"') {
        return Some(quoted_string_end(bytes, index));
    }
    if bytes.get(index..index + 3) == Some(b"///") {
        return Some(raw_string_end(bytes, index));
    }
    if bytes.get(index..index + 2) == Some(b"--") {
        return Some(line_comment_end(bytes, index));
    }
    if bytes.get(index..index + 2) == Some(b"-*") {
        return Some(block_comment_end(bytes, index));
    }
    None
}

fn quoted_string_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn raw_string_end(bytes: &[u8], start: usize) -> usize {
    find_delimiter_end(bytes, start + 3, b"///")
}

fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    bytes[start + 2..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| start + 2 + offset)
}

fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    find_delimiter_end(bytes, start + 2, b"*-")
}

fn find_delimiter_end(bytes: &[u8], mut index: usize, delimiter: &[u8]) -> usize {
    while index + delimiter.len() <= bytes.len() {
        if &bytes[index..index + delimiter.len()] == delimiter {
            return index + delimiter.len();
        }
        index += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_complete_nested_macros_without_moving_source_positions() {
        let text = "x = $outer $inner x $ $\ny = 1\n";
        let syntax = MacroSyntax::scan(text);

        assert_eq!(syntax.parse_text(), "x =  outer  inner x    \ny = 1\n");
        assert_eq!(syntax.parse_text().len(), text.len());
        assert!(syntax.is_macro_name(5, 10));
        assert!(syntax.is_macro_name(12, 17));
    }

    #[test]
    fn leaves_macro_text_in_strings_and_comments_opaque() {
        let text = concat!(
            "a = \"$fake 1 $\"\n",
            "b = ///$raw 2 $///\n",
            "-- $line 3 $\n",
            "-* $block 4 $ *-\n",
            "c = $real 5 $\n",
        );
        let syntax = MacroSyntax::scan(text);

        assert!(syntax.parse_text().contains("\"$fake 1 $\""));
        assert!(syntax.parse_text().contains("///$raw 2 $///"));
        assert!(syntax.parse_text().contains("-- $line 3 $"));
        assert!(syntax.parse_text().contains("-* $block 4 $ *-"));
        assert!(syntax.parse_text().contains("c =  real 5  "));
    }

    #[test]
    fn leaves_an_unterminated_outer_macro_for_parser_diagnostics() {
        let text = "x = $outer $inner 1 $";
        let syntax = MacroSyntax::scan(text);

        assert_eq!(syntax.parse_text(), text);
        assert!(!syntax.has_macros());
    }
}
