//! Footnote and endnote numbering (§17.11).
//!
//! A note's number is its reference's place in the document (§17.11.19
//! `numRestart`: continuous by default, or restarting in each section or on
//! each page), from `numStart`, written in `numFmt` (§17.11.17; footnotes
//! default to decimal, endnotes to lowerRoman). The section's own
//! `footnotePr`/`endnotePr` override the document's in `settings.xml`. The
//! numbers are fixed at load, in document order, so the mark in the text and
//! the mark in the note always agree; per-page restarts are applied once the
//! pages are known (`mark_on_page`).
//!
//! A reference with `customMarkFollows` takes no number and does not advance
//! the count: the document writes its mark itself (§17.11.14).

use std::cell::RefCell;
use std::collections::HashMap;

use ooxml_wml::types::{BlockContent, RunContent, STRestartNumber};

use crate::engine::counter::{self, CounterStyle};
use crate::wml::content_walk::{self, Visit};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NoteKind {
    Foot,
    End,
}

/// How a section numbers one kind of note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoteProps {
    pub(crate) style: CounterStyle,
    pub(crate) start: i64,
    pub(crate) restart: STRestartNumber,
}

impl NoteProps {
    fn spec_default(kind: NoteKind) -> Self {
        let style = match kind {
            NoteKind::Foot => CounterStyle::Decimal,
            NoteKind::End => CounterStyle::LowerRoman,
        };
        Self { style, start: 1, restart: STRestartNumber::Continuous }
    }

    /// `base` with the numbering elements a `footnotePr`/`endnotePr` sets.
    fn overlay(
        mut self,
        num_fmt: Option<&ooxml_wml::types::CTNumFmt>,
        num_start: Option<&ooxml_wml::types::CTDecimalNumber>,
        num_restart: Option<&ooxml_wml::types::CTNumRestart>,
    ) -> Self {
        if let Some(f) = num_fmt {
            self.style = crate::wml::numbering::counter_style(&f.value, f.format.as_deref());
        }
        if let Some(s) = num_start {
            self.start = s.value;
        }
        if let Some(r) = num_restart {
            self.restart = r.value;
        }
        self
    }
}

/// The numbering of a section's footnotes and endnotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SectionNotes {
    pub(crate) foot: NoteProps,
    pub(crate) end: NoteProps,
}

impl SectionNotes {
    /// The document's note numbering, from the `footnotePr` and `endnotePr`
    /// of `settings.xml` (§17.11.12, §17.11.4). Read element by element:
    /// the settings part holds much the generated parser need not accept.
    pub(crate) fn from_settings(settings_xml: Option<&[u8]>) -> Self {
        use ooxml_xml::FromXml;
        use quick_xml::events::Event;
        let (mut foot, mut end) = (None, None);
        if let Some(xml) = settings_xml {
            let mut reader = quick_xml::Reader::from_reader(std::io::Cursor::new(xml));
            let mut buf = Vec::new();
            loop {
                let (e, empty) = match reader.read_event_into(&mut buf) {
                    Ok(Event::Start(e)) => (e.into_owned(), false),
                    Ok(Event::Empty(e)) => (e.into_owned(), true),
                    Ok(Event::Eof) | Err(_) => break,
                    _ => continue,
                };
                match e.local_name().as_ref() {
                    b"footnotePr" => foot = ooxml_wml::types::CTFtnDocProps::from_xml(&mut reader, &e, empty).ok(),
                    b"endnotePr" => end = ooxml_wml::types::CTEdnDocProps::from_xml(&mut reader, &e, empty).ok(),
                    _ => {}
                }
                buf.clear();
            }
        }
        Self::document(foot.as_ref(), end.as_ref())
    }

    fn document(
        foot: Option<&ooxml_wml::types::CTFtnDocProps>,
        end: Option<&ooxml_wml::types::CTEdnDocProps>,
    ) -> Self {
        Self {
            foot: NoteProps::spec_default(NoteKind::Foot).overlay(
                foot.and_then(|p| p.num_fmt.as_deref()),
                foot.and_then(|p| p.num_start.as_deref()),
                foot.and_then(|p| p.num_restart.as_deref()),
            ),
            end: NoteProps::spec_default(NoteKind::End).overlay(
                end.and_then(|p| p.num_fmt.as_deref()),
                end.and_then(|p| p.num_start.as_deref()),
                end.and_then(|p| p.num_restart.as_deref()),
            ),
        }
    }

    /// A section's numbering: the document's, with its sectPr's overrides.
    pub(crate) fn section(self, sect_pr: &ooxml_wml::types::SectionProperties) -> Self {
        let foot = sect_pr.footnote_pr.as_deref();
        let end = sect_pr.endnote_pr.as_deref();
        Self {
            foot: self.foot.overlay(
                foot.and_then(|p| p.num_fmt.as_deref()),
                foot.and_then(|p| p.num_start.as_deref()),
                foot.and_then(|p| p.num_restart.as_deref()),
            ),
            end: self.end.overlay(
                end.and_then(|p| p.num_fmt.as_deref()),
                end.and_then(|p| p.num_start.as_deref()),
                end.and_then(|p| p.num_restart.as_deref()),
            ),
        }
    }

    fn of(&self, kind: NoteKind) -> NoteProps {
        match kind {
            NoteKind::Foot => self.foot,
            NoteKind::End => self.end,
        }
    }
}

/// A note's mark: its number and how it is written, or none for a
/// reference that carries its own mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Number { n: i64, props: NoteProps },
    Custom,
}

/// Every note's mark, fixed at load.
#[derive(Clone, Debug, Default)]
pub(crate) struct NoteMarks {
    marks: HashMap<(NoteKind, i64), Mark>,
}

impl NoteMarks {
    /// Number the references in `sections` (each section's blocks and note
    /// numbering), in document order.
    pub(crate) fn number<'a>(sections: impl IntoIterator<Item = (&'a [BlockContent], SectionNotes)>) -> Self {
        let mut marks = HashMap::new();
        let mut next: HashMap<NoteKind, i64> = HashMap::new();
        for (sec_idx, (blocks, notes)) in sections.into_iter().enumerate() {
            for kind in [NoteKind::Foot, NoteKind::End] {
                let props = notes.of(kind);
                if sec_idx == 0 || props.restart == STRestartNumber::EachSect {
                    next.insert(kind, props.start);
                }
            }
            for p in content_walk::all_paragraphs(blocks) {
                content_walk::walk(&p.paragraph_content, Default::default(), &mut |visit, _| {
                    let Visit::Run(r) = visit else { return };
                    for rc in &r.run_content {
                        let (kind, reference) = match rc {
                            RunContent::FootnoteReference(f) => (NoteKind::Foot, f),
                            RunContent::EndnoteReference(e) => (NoteKind::End, e),
                            _ => continue,
                        };
                        let custom = reference.custom_mark_follows.as_deref().is_some_and(crate::on_off);
                        let mark = if custom {
                            Mark::Custom
                        } else {
                            let counter = next.entry(kind).or_insert(1);
                            let n = *counter;
                            *counter += 1;
                            Mark::Number { n, props: notes.of(kind) }
                        };
                        marks.entry((kind, reference.id)).or_insert(mark);
                    }
                });
            }
        }
        Self { marks }
    }
}

thread_local! {
    static NOTE_MARKS: RefCell<NoteMarks> = RefCell::new(NoteMarks::default());
    /// The note being laid out and the mark its `footnoteRef`/`endnoteRef`
    /// writes; `None` outside a note.
    static CURRENT_NOTE: RefCell<Option<(NoteKind, Option<String>)>> = const { RefCell::new(None) };
}

/// Make `marks` the marks the layout writes.
pub(crate) fn publish(marks: &NoteMarks) {
    NOTE_MARKS.with(|m| *m.borrow_mut() = marks.clone());
}

/// The mark of note `id`, or `None` for a custom mark or an unknown note.
/// A note numbered per page is written here with its document number;
/// `mark_on_page` gives the page's.
pub(crate) fn mark(kind: NoteKind, id: i64) -> Option<String> {
    NOTE_MARKS.with(|m| match m.borrow().marks.get(&(kind, id)) {
        Some(Mark::Number { n, props }) => Some(counter::format(*n, props.style)),
        _ => None,
    })
}

/// The mark of note `id` on a page whose notes of this kind are `on_page`,
/// in reference order: under `eachPage` its place there, from its start.
pub(crate) fn mark_on_page(kind: NoteKind, id: i64, on_page: &[i64]) -> Option<String> {
    NOTE_MARKS.with(|m| {
        let marks = m.borrow();
        match marks.marks.get(&(kind, id)) {
            Some(Mark::Number { props, .. }) if props.restart == STRestartNumber::EachPage => {
                let numbered = on_page.iter().filter(|other| {
                    matches!(marks.marks.get(&(kind, **other)), Some(Mark::Number { .. }))
                });
                let place = numbered.take_while(|other| **other != id).count() as i64;
                Some(counter::format(props.start + place, props.style))
            }
            Some(Mark::Number { n, props }) => Some(counter::format(*n, props.style)),
            _ => None,
        }
    })
}

/// Run `f` laying out a note of `kind`, whose own mark (`footnoteRef`)
/// writes `mark`: the mark its reference shows on the same page.
pub(crate) fn within_note<T>(kind: NoteKind, mark: Option<String>, f: impl FnOnce() -> T) -> T {
    let previous = CURRENT_NOTE.with(|c| c.replace(Some((kind, mark))));
    let out = f();
    CURRENT_NOTE.with(|c| *c.borrow_mut() = previous);
    out
}

/// The mark a note's own `footnoteRef`/`endnoteRef` of `kind` writes, when
/// such a note is being laid out.
pub(crate) fn own_mark(kind: NoteKind) -> Option<String> {
    CURRENT_NOTE.with(|c| match &*c.borrow() {
        Some((k, mark)) if *k == kind => mark.clone(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endnotes_default_to_lower_roman_and_custom_marks_take_no_number() {
        let doc = SectionNotes::from_settings(None);
        let set = SectionNotes::from_settings(Some(
            br#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:endnotePr><w:numFmt w:val="decimal"/><w:numStart w:val="5"/></w:endnotePr></w:settings>"#,
        ));
        assert_eq!((set.end.style, set.end.start), (CounterStyle::Decimal, 5));
        assert_eq!(doc.end.style, CounterStyle::LowerRoman);
        assert_eq!(doc.foot.style, CounterStyle::Decimal);
        let marks = NoteMarks {
            marks: HashMap::from([
                ((NoteKind::Foot, 1), Mark::Number { n: 1, props: doc.foot }),
                ((NoteKind::Foot, 2), Mark::Custom),
                ((NoteKind::Foot, 3), Mark::Number { n: 2, props: NoteProps { restart: STRestartNumber::EachPage, ..doc.foot } }),
            ]),
        };
        publish(&marks);
        assert_eq!(mark(NoteKind::Foot, 1).as_deref(), Some("1"));
        assert_eq!(mark(NoteKind::Foot, 2), None);
        // Per page: note 3 is the first numbered note on its page.
        assert_eq!(mark_on_page(NoteKind::Foot, 3, &[2, 3]).as_deref(), Some("1"));
    }
}
