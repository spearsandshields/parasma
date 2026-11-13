//! Conservative raw-XML byte-scanning primitives shared by the repair rules.
//!
//! Every rule rewrites only the deficient start tags (or element spans) and
//! copies everything else byte-for-byte: no entity decoding (the targeted attrs
//! are numeric), no reordering, no removal beyond the rule's documented span.
//! These helpers are the common machinery: locate a start tag's `>` while
//! honoring quoted attribute values, test whether an attribute is present as a
//! whole word, and walk UTF-8 safely past a non-`<` byte.

/// Byte offset (relative to the tag start `b[0] == b'<'`) of the `>` that closes
/// this start tag, honoring single/double-quoted attribute values so a `>` inside
/// a quoted value is not mistaken for the tag end. `None` if unterminated.
///
/// **`None` is final for the caller's scan.** It means this call read to the end
/// of the input, so a loop that steps one character and tries the next `<`
/// re-reads the whole tail each time: quadratic on a part with many unterminated
/// tags (400,000 of them took minutes). A caller that gets `None` must copy the
/// remainder through unchanged and stop. The input is malformed at that point
/// and the parser will reject it; nothing is lost by not repairing further.
pub(crate) fn find_tag_end(b: &[u8]) -> Option<usize> {
    let mut i = 1;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'>' => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// True if attribute `name` appears in `tag` as a whole attribute name
/// (`name=`), i.e. preceded by whitespace or the tag opener, so `name` does not
/// match inside a longer attribute name (e.g. `min` must not match `mine=`).
pub(crate) fn attr_present(tag: &str, name: &str) -> bool {
    let needle = format!("{name}=");
    let mut search = 0;
    while let Some(pos) = tag[search..].find(&needle) {
        let abs = search + pos;
        let ok_before = abs == 0
            || matches!(tag.as_bytes()[abs - 1], b' ' | b'\t' | b'\r' | b'\n');
        if ok_before {
            return true;
        }
        search = abs + needle.len();
    }
    false
}

/// UTF-8 byte length of the char starting at lead byte `b` (1 on a stray
/// continuation byte, so the scanner always advances).
pub(crate) fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}
