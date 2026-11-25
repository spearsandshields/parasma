//! Markup Compatibility (ECMA-376 Part 3): resolve every `mc:AlternateContent`
//! in a WordprocessingML package before the parser reads it.
//!
//! Word 2010 and later write each text box, shape, group and canvas twice,
//! inside an `AlternateContent` element: a `Choice` in the newer markup that
//! only newer readers understand, and a `Fallback` in older markup (Part 3
//! §7.5-7.7). The parser has no model for the element, so it keeps the whole
//! thing as an unrecognised child that no reader looks into, and the content
//! vanished: a corpus probe found the text of 1 of 51 such text boxes on the
//! page.
//!
//! This pass applies the selection rule of Part 3 §9.3 to the package's XML
//! parts and splices the selected branch's children in place of the element,
//! so every later reader (layout, image and font collectors, headers, notes,
//! text-box contents) sees ordinary markup:
//! - the first `Choice` whose `Requires` namespaces are all understood is
//!   selected;
//! - failing that, the `Fallback`;
//! - failing both, the element is removed.
//!
//! "Understood" means the engine draws that markup: [`UNDERSTOOD`] lists the
//! namespaces. A namespace is added when its content renders at least as well
//! as the fallback.
//!
//! Deviations, declared: the other MCE attributes (`Ignorable`,
//! `ProcessContent`, `MustUnderstand`) are not processed (the parser already
//! tolerates unknown markup), and a `Requires` prefix is resolved against the
//! declarations met earlier in the part, in document order, rather than only
//! those in scope at the element. Word declares them all on the root element.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::ops::Range;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// Namespaces whose markup the engine draws, so a `Choice` that requires only
/// these is selected over its `Fallback`.
const UNDERSTOOD: &[&str] = &[
    // Word 2010 shapes and text boxes (`wps:wsp`), read by
    // `wml::drawing::extract_drawing_text_box`.
    "http://schemas.microsoft.com/office/word/2010/wordprocessingShape",
];

/// Largest buffer reserved up front from a zip entry's declared size, which
/// the file's author controls. Reads still grow past it as real data arrives.
const PREALLOC_CAP: u64 = 16 * 1024 * 1024;

/// Resolve every `AlternateContent` in the package's WordprocessingML parts.
/// Returns the bytes unchanged when no part holds one, or when the package or
/// a part cannot be read or rewritten (the parse then sees the original, as
/// before this pass existed).
pub(crate) fn resolve_package(bytes: Vec<u8>) -> Vec<u8> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes.as_slice())) else {
        return bytes;
    };
    let mut replacements: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else { continue };
        let Ok(name) = entry.name().map(|n| n.into_owned()) else { continue };
        if !(name.starts_with("word/") && name.ends_with(".xml")) {
            continue;
        }
        let mut xml = Vec::with_capacity(entry.size().min(PREALLOC_CAP) as usize);
        if entry.read_to_end(&mut xml).is_err() {
            continue;
        }
        if let Some(resolved) = resolve_part(&xml) {
            replacements.push((name, resolved));
        }
    }
    if replacements.is_empty() {
        return bytes;
    }
    rewrite_parts(&bytes, &replacements).unwrap_or(bytes)
}

/// One part with every `AlternateContent` resolved, or `None` when it holds
/// none or does not read as XML.
pub(crate) fn resolve_part(xml: &[u8]) -> Option<Vec<u8>> {
    if !contains(xml, b"AlternateContent") {
        return None;
    }
    let resolved = resolve_fragment(xml, &HashMap::new())?;
    (resolved != xml).then_some(resolved)
}

/// `xml` with each top-level `AlternateContent` replaced by its selected
/// branch's children, themselves resolved. `outer` holds the namespace
/// declarations met before this fragment; the fragment's own are added as they
/// are read, so the root element's are known before any `AlternateContent`.
fn resolve_fragment(xml: &[u8], outer: &HashMap<String, String>) -> Option<Vec<u8>> {
    let mut namespaces = outer.clone();
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = false;
    let mut splices: Vec<(Range<usize>, Vec<u8>)> = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        match reader.read_event().ok()? {
            Event::Start(e) if local_name(&e) == b"AlternateContent" => {
                let (selected, end) = select_branch(&mut reader, &namespaces)?;
                let inner = resolve_fragment(&xml[selected], &namespaces)?;
                splices.push((before..end, inner));
            }
            Event::Empty(e) if local_name(&e) == b"AlternateContent" => {
                splices.push((before..reader.buffer_position() as usize, Vec::new()));
            }
            Event::Start(e) | Event::Empty(e) => declare_namespaces(&e, &mut namespaces),
            Event::Eof => break,
            _ => {}
        }
    }
    let mut out = Vec::with_capacity(xml.len());
    let mut at = 0;
    for (range, with) in splices {
        out.extend_from_slice(&xml[at..range.start]);
        out.extend_from_slice(&with);
        at = range.end;
    }
    out.extend_from_slice(&xml[at..]);
    Some(out)
}

/// Having just read an `AlternateContent` start tag, read to its end tag and
/// return the byte range of the selected branch's content (empty when nothing
/// is selected) and the offset just past the end tag (Part 3 §9.3).
fn select_branch(
    reader: &mut Reader<&[u8]>,
    namespaces: &HashMap<String, String>,
) -> Option<(Range<usize>, usize)> {
    let mut choice: Option<Range<usize>> = None;
    let mut fallback: Option<Range<usize>> = None;
    loop {
        let after_start = |r: &Reader<&[u8]>| r.buffer_position() as usize;
        match reader.read_event().ok()? {
            Event::Start(e) => {
                let local = local_name(&e).to_vec();
                let start = after_start(reader);
                let end = skip_element(reader)?;
                match local.as_slice() {
                    b"Choice" if choice.is_none() && requires_understood(&e, namespaces) => {
                        choice = Some(start..end);
                    }
                    b"Fallback" if fallback.is_none() => fallback = Some(start..end),
                    _ => {}
                }
            }
            Event::Empty(e) => {
                let at = after_start(reader);
                match local_name(&e) {
                    b"Choice" if choice.is_none() && requires_understood(&e, namespaces) => {
                        choice = Some(at..at);
                    }
                    b"Fallback" if fallback.is_none() => fallback = Some(at..at),
                    _ => {}
                }
            }
            Event::End(_) => {
                let end = reader.buffer_position() as usize;
                return Some((choice.or(fallback).unwrap_or(end..end), end));
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// Having just read a start tag, read to its matching end tag and return the
/// offset where that end tag begins: the end of the element's content.
fn skip_element(reader: &mut Reader<&[u8]>) -> Option<usize> {
    let mut depth = 1usize;
    loop {
        let before = reader.buffer_position() as usize;
        match reader.read_event().ok()? {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Some(before);
                }
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// Whether every namespace a `Choice` requires is one the engine draws. The
/// element's own declarations count, then the part's.
fn requires_understood(choice: &BytesStart, namespaces: &HashMap<String, String>) -> bool {
    let mut own = HashMap::new();
    let mut requires = None;
    for attr in choice.attributes().flatten() {
        let key = attr.key.as_ref();
        let value = String::from_utf8_lossy(&attr.value).into_owned();
        if let Some(prefix) = key.strip_prefix(b"xmlns:") {
            own.insert(String::from_utf8_lossy(prefix).into_owned(), value);
        } else if key == b"Requires" {
            requires = Some(value);
        }
    }
    let Some(requires) = requires else { return false };
    let mut prefixes = requires.split_whitespace().peekable();
    prefixes.peek().is_some()
        && prefixes.all(|p| {
            own.get(p)
                .or_else(|| namespaces.get(p))
                .is_some_and(|uri| UNDERSTOOD.contains(&uri.as_str()))
        })
}

/// Record an element's `xmlns:prefix` declarations, keeping the first one met
/// for each prefix.
fn declare_namespaces(e: &BytesStart, namespaces: &mut HashMap<String, String>) {
    for attr in e.attributes().flatten() {
        if let Some(prefix) = attr.key.as_ref().strip_prefix(b"xmlns:") {
            namespaces
                .entry(String::from_utf8_lossy(prefix).into_owned())
                .or_insert_with(|| String::from_utf8_lossy(&attr.value).into_owned());
        }
    }
}

fn local_name<'a>(e: &'a BytesStart) -> &'a [u8] {
    let name = e.name().into_inner();
    name.iter().rposition(|&b| b == b':').map_or(name, |i| &name[i + 1..])
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Rebuild a zip package with the named parts' bytes replaced. Every other
/// entry is copied as it is, still compressed, so images are not inflated; the
/// replaced parts are stored uncompressed, since the package is parsed in
/// memory and discarded (compressing a 12 MB part cost 120 ms for nothing).
/// `None` on any read or write error.
///
/// The `repair/` module has its own rebuild, which inflates every entry; it is
/// deliberately not shared, because that module is temporary (see its header).
fn rewrite_parts(original: &[u8], replacements: &[(String, Vec<u8>)]) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(original)).ok()?;
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::with_capacity(original.len())));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    for i in 0..archive.len() {
        let name = archive.by_index_raw(i).ok()?.name().ok()?.into_owned();
        match replacements.iter().find(|(part, _)| *part == name) {
            Some((_, bytes)) => {
                out.start_file(name, options).ok()?;
                out.write_all(bytes).ok()?;
            }
            None => out.raw_copy_file(archive.by_index_raw(i).ok()?).ok()?,
        }
    }
    Some(out.finish().ok()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_OPEN: &str = r#"<w:document xmlns:w="w" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:wpg="http://schemas.microsoft.com/office/word/2010/wordprocessingGroup" xmlns:v="v">"#;

    fn resolve(body: &str) -> String {
        let xml = format!("{ROOT_OPEN}{body}</w:document>");
        let out = resolve_part(xml.as_bytes()).map_or(xml, |b| String::from_utf8(b).unwrap());
        out.strip_prefix(ROOT_OPEN).unwrap().strip_suffix("</w:document>").unwrap().to_string()
    }

    #[test]
    fn an_understood_choice_is_selected() {
        let ac = r#"<w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing>NEW</w:drawing></mc:Choice><mc:Fallback><w:pict>OLD</w:pict></mc:Fallback></mc:AlternateContent></w:r>"#;
        assert_eq!(resolve(ac), "<w:r><w:drawing>NEW</w:drawing></w:r>");
    }

    #[test]
    fn a_choice_needing_anything_not_understood_falls_back() {
        let ac = r#"<mc:AlternateContent><mc:Choice Requires="wpg"><a/></mc:Choice><mc:Fallback><w:pict>OLD</w:pict></mc:Fallback></mc:AlternateContent>"#;
        assert_eq!(resolve(ac), "<w:pict>OLD</w:pict>");
        let both = r#"<mc:AlternateContent><mc:Choice Requires="wps wpg"><a/></mc:Choice><mc:Fallback><b/></mc:Fallback></mc:AlternateContent>"#;
        assert_eq!(resolve(both), "<b/>", "every required namespace must be understood");
        let unknown = r#"<mc:AlternateContent><mc:Choice Requires="nope"><a/></mc:Choice><mc:Fallback><b/></mc:Fallback></mc:AlternateContent>"#;
        assert_eq!(resolve(unknown), "<b/>", "an undeclared prefix is not understood");
    }

    #[test]
    fn the_first_understood_choice_wins_and_nothing_selected_means_removed() {
        let ac = r#"<mc:AlternateContent><mc:Choice Requires="wpg"><a/></mc:Choice><mc:Choice Requires="wps"><b/></mc:Choice><mc:Choice Requires="wps"><c/></mc:Choice></mc:AlternateContent>"#;
        assert_eq!(resolve(ac), "<b/>");
        let none = r#"x<mc:AlternateContent><mc:Choice Requires="wpg"><a/></mc:Choice></mc:AlternateContent>y"#;
        assert_eq!(resolve(none), "xy");
        assert_eq!(resolve("x<mc:AlternateContent/>y"), "xy");
    }

    #[test]
    fn a_prefix_declared_on_the_choice_itself_counts() {
        let ac = r#"<mc:AlternateContent><mc:Choice xmlns:s="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" Requires="s"><a/></mc:Choice><mc:Fallback><b/></mc:Fallback></mc:AlternateContent>"#;
        assert_eq!(resolve(ac), "<a/>");
    }

    #[test]
    fn nested_alternate_content_is_resolved_inside_the_selection() {
        let ac = r#"<mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><mc:AlternateContent><mc:Choice Requires="wpg"><a/></mc:Choice><mc:Fallback><inner/></mc:Fallback></mc:AlternateContent></w:drawing></mc:Choice><mc:Fallback/></mc:AlternateContent>"#;
        assert_eq!(resolve(ac), "<w:drawing><inner/></w:drawing>");
    }

    #[test]
    fn untouched_bytes_stay_byte_identical() {
        let around = "<w:p><w:r><w:t xml:space=\"preserve\"> a &amp; b </w:t></w:r></w:p>";
        let ac = r#"<mc:AlternateContent><mc:Choice Requires="wps"><x/></mc:Choice></mc:AlternateContent>"#;
        assert_eq!(resolve(&format!("{around}{ac}{around}")), format!("{around}<x/>{around}"));
        let xml = format!("{ROOT_OPEN}{around}</w:document>");
        assert!(resolve_part(xml.as_bytes()).is_none(), "a part with no AlternateContent is left alone");
    }

    #[test]
    fn unreadable_xml_is_left_alone() {
        assert!(resolve_part(b"<a><mc:AlternateContent><mc:Choice Requires=\"wps\">").is_none());
    }
}
