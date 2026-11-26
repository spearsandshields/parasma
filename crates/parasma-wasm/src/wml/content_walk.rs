//! One walk over a paragraph's content, at any depth.
//!
//! A paragraph's runs can sit inside any nesting of wrappers: hyperlinks
//! (§17.16.22), simple fields (§17.16.19), run-level content controls
//! (§17.5.2.31), smart tags (§17.5.1.9), custom XML (§17.5.1.3), direction and
//! bidi overrides (§17.3.2.8, §17.3.2.3), and tracked insertions, deletions and
//! moves (§17.13.5). Readers that only looked at direct runs, or at one wrapper
//! kind one level deep, dropped the rest: a corpus probe found no text inside
//! smart tags at all. Layout and the load-time collectors (fonts, code points,
//! images, watermarks) now all walk the content through this one function, so
//! they agree on which runs a paragraph has. [`flatten_blocks`] does the same
//! for the blocks of a body, cell, header or text box.

use ooxml_wml::types::{
    Bookmark, CTRunTrackChange, CTSimpleField, Hyperlink, Paragraph, ParagraphContent, Run,
    RunContentChoice,
};

/// What a tracked change does to the runs inside it (§17.13.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Revision {
    /// `w:ins` or `w:moveTo`: text added here.
    Inserted,
    /// `w:del` or `w:moveFrom`: text removed from here.
    Deleted,
}

/// The wrappers a piece of content sits in, innermost first.
#[derive(Clone, Copy, Default)]
pub(crate) struct Context<'a> {
    pub(crate) hyperlink: Option<&'a Hyperlink>,
    pub(crate) simple_field: Option<&'a CTSimpleField>,
    /// The tracked change and its author. Nested changes keep the outer one,
    /// so text inserted inside a deletion draws as deleted.
    pub(crate) revision: Option<(Revision, &'a str)>,
}

/// One thing the walk reports, with the context it was found in.
pub(crate) enum Visit<'a> {
    Run(&'a Run),
    BookmarkStart(&'a Bookmark),
    CommentRangeStart(i64),
    CommentRangeEnd(i64),
}

/// Walk `content` in document order, reporting each run and marker with its
/// context.
pub(crate) fn walk<'a>(
    content: &'a [ParagraphContent],
    ctx: Context<'a>,
    visit: &mut impl FnMut(Visit<'a>, Context<'a>),
) {
    for item in content {
        walk_one(item, ctx, visit);
    }
}

/// [`walk`] for one item.
pub(crate) fn walk_one<'a>(
    item: &'a ParagraphContent,
    ctx: Context<'a>,
    visit: &mut impl FnMut(Visit<'a>, Context<'a>),
) {
    use ParagraphContent as P;
    match item {
        P::R(r) => visit(Visit::Run(r), ctx),
        P::BookmarkStart(b) => visit(Visit::BookmarkStart(b), ctx),
        P::CommentRangeStart(c) => visit(Visit::CommentRangeStart(c.id), ctx),
        P::CommentRangeEnd(c) => visit(Visit::CommentRangeEnd(c.id), ctx),
        P::Hyperlink(h) => walk(&h.paragraph_content, Context { hyperlink: Some(h), ..ctx }, visit),
        P::FldSimple(f) => walk(&f.paragraph_content, Context { simple_field: Some(f), ..ctx }, visit),
        P::Sdt(s) => {
            if let Some(content) = &s.sdt_content {
                walk(&content.paragraph_content, ctx, visit);
            }
        }
        P::SmartTag(s) => walk(&s.paragraph_content, ctx, visit),
        P::CustomXml(c) => walk(&c.paragraph_content, ctx, visit),
        P::Dir(d) => walk(&d.paragraph_content, ctx, visit),
        P::Bdo(b) => walk(&b.paragraph_content, ctx, visit),
        P::Ins(c) | P::MoveTo(c) => walk_revision(c, Revision::Inserted, ctx, visit),
        P::Del(c) | P::MoveFrom(c) => walk_revision(c, Revision::Deleted, ctx, visit),
        _ => {}
    }
}

fn walk_revision<'a>(
    change: &'a CTRunTrackChange,
    kind: Revision,
    ctx: Context<'a>,
    visit: &mut impl FnMut(Visit<'a>, Context<'a>),
) {
    let ctx = Context { revision: ctx.revision.or(Some((kind, change.author.as_str()))), ..ctx };
    for item in &change.run_content {
        use RunContentChoice as C;
        match item {
            C::R(r) => visit(Visit::Run(r), ctx),
            C::BookmarkStart(b) => visit(Visit::BookmarkStart(b), ctx),
            C::CommentRangeStart(c) => visit(Visit::CommentRangeStart(c.id), ctx),
            C::CommentRangeEnd(c) => visit(Visit::CommentRangeEnd(c.id), ctx),
            C::Sdt(s) => {
                if let Some(content) = &s.sdt_content {
                    walk(&content.paragraph_content, ctx, visit);
                }
            }
            C::SmartTag(s) => walk(&s.paragraph_content, ctx, visit),
            C::CustomXml(c) => walk(&c.paragraph_content, ctx, visit),
            C::Dir(d) => walk(&d.paragraph_content, ctx, visit),
            C::Bdo(b) => walk(&b.paragraph_content, ctx, visit),
            C::Ins(c) | C::MoveTo(c) => walk_revision(c, Revision::Inserted, ctx, visit),
            C::Del(c) | C::MoveFrom(c) => walk_revision(c, Revision::Deleted, ctx, visit),
            _ => {}
        }
    }
}

// ─── Blocks ──────────────────────────────────────────────────────────────────

/// A block resolved to its renderable form, with its wrappers unwrapped.
pub(crate) enum FlatBlock<'a> {
    Para(&'a ooxml_wml::types::Paragraph),
    Table(&'a ooxml_wml::types::Table),
}

/// Flatten `blocks`, descending into block-level content controls
/// (§17.5.2.29) and custom XML (§17.5.1.6) at any depth, so their paragraphs
/// and tables are laid out. Word wraps cover pages, header page numbers and
/// built-in text-box content in a control. Other block markup (bookmarks,
/// ranges) is skipped.
pub(crate) fn flatten_blocks<'a>(blocks: &'a [ooxml_wml::types::BlockContent], out: &mut Vec<FlatBlock<'a>>) {
    use ooxml_wml::types::BlockContent as B;
    for b in blocks {
        match b {
            B::P(p) => out.push(FlatBlock::Para(p)),
            B::Tbl(t) => out.push(FlatBlock::Table(t)),
            B::Sdt(sdt) => {
                if let Some(content) = &sdt.sdt_content {
                    flatten_block_choices(&content.block_content, out);
                }
            }
            B::CustomXml(c) => flatten_block_choices(&c.block_content, out),
            _ => {}
        }
    }
}

/// The content of a block wrapper is a sibling enum (`BlockContentChoice`);
/// same flattening.
fn flatten_block_choices<'a>(
    blocks: &'a [ooxml_wml::types::BlockContentChoice],
    out: &mut Vec<FlatBlock<'a>>,
) {
    use ooxml_wml::types::BlockContentChoice as BC;
    for b in blocks {
        match b {
            BC::P(p) => out.push(FlatBlock::Para(p)),
            BC::Tbl(t) => out.push(FlatBlock::Table(t)),
            BC::Sdt(sdt) => {
                if let Some(content) = &sdt.sdt_content {
                    flatten_block_choices(&content.block_content, out);
                }
            }
            BC::CustomXml(c) => flatten_block_choices(&c.block_content, out),
            _ => {}
        }
    }
}

/// Every paragraph in `blocks`, in document order: through block wrappers and
/// into every table cell, nested tables included. The load-time collectors
/// (images, charts, fonts, code points, math) read the document through it,
/// so content inside a cover page's content control is not missed.
pub(crate) fn all_paragraphs(blocks: &[ooxml_wml::types::BlockContent]) -> Vec<&Paragraph> {
    fn walk_blocks<'a>(blocks: &'a [ooxml_wml::types::BlockContent], out: &mut Vec<&'a Paragraph>) {
        let mut flat = Vec::new();
        flatten_blocks(blocks, &mut flat);
        for block in flat {
            match block {
                FlatBlock::Para(p) => out.push(p),
                FlatBlock::Table(t) => {
                    for row in crate::wml::table::table_rows(t) {
                        for cell in crate::wml::table::collect_row_cells(row) {
                            walk_blocks(&cell.block_content, out);
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk_blocks(blocks, &mut out);
    out
}

/// Every run of `paragraph`, at any depth, in document order.
pub(crate) fn paragraph_runs(paragraph: &Paragraph) -> Vec<&Run> {
    let mut runs = Vec::new();
    walk(&paragraph.paragraph_content, Context::default(), &mut |v, _| {
        if let Visit::Run(r) = v {
            runs.push(r);
        }
    });
    runs
}
