//! WML package-loader plumbing. The helpers `load_docx` calls to
//! walk the parsed `Document` at load time: building the section model
//! (§17.6 `w:sectPr`), loading header/footer parts (§17.10) and their images,
//! building the numbering lookup (§17.9), and extracting core-property dates
//! (`docProps/core.xml`). The `Section`/`SectionHdrFtr` model types live here
//! too — they carry header/footer block content (§17.10). Verbatim relocation
//! from `lib.rs`; `load_docx` itself (a `#[wasm_bindgen]` `Viewer` method)
//! stays in `lib.rs` and calls these via `wml::loader::*`.

use std::collections::HashMap;

use ooxml_wml::ext::{BodyExt, SectionPropertiesExt};
use ooxml_wml::types::{BlockContent, HeaderFooterRef, STHdrFtr, STNumberFormat};

use crate::engine::images::ImageBytes;
use crate::field;
use crate::wml::numbering::{MarkerRunOverrides, NumberingLevelDef};
use crate::wml::section::{PageGeometry, PageNumbering, SectionBreakType};

// ─── Section model ─────────────────────────────────────────────────────────

pub(crate) struct SectionHdrFtr {
    pub(crate) default_header: Option<Vec<BlockContent>>,
    pub(crate) first_header: Option<Vec<BlockContent>>,
    pub(crate) even_header: Option<Vec<BlockContent>>,
    pub(crate) default_footer: Option<Vec<BlockContent>>,
    pub(crate) first_footer: Option<Vec<BlockContent>>,
    pub(crate) even_footer: Option<Vec<BlockContent>>,
    /// Part paths for each loaded header/footer (for resolving part-level rels).
    pub(crate) part_paths: Vec<String>,
    /// The part each slot above came from, in the same order (default, first,
    /// even header; default, first, even footer), so a slot's image ids are
    /// read against its own part's relationships.
    pub(crate) slot_parts: [Option<String>; 6],
    pub(crate) title_pg: bool,
    pub(crate) even_odd: bool,
}

pub(crate) struct Section {
    pub(crate) blocks: Vec<BlockContent>,
    pub(crate) geometry: PageGeometry,
    pub(crate) hdr_ftr: SectionHdrFtr,
    pub(crate) break_type: SectionBreakType,
    pub(crate) page_numbering: PageNumbering,
    /// How the section numbers its footnotes and endnotes (§17.11).
    pub(crate) notes: crate::wml::notes::SectionNotes,
}

/// Extract a core-property date (`dcterms:created`/`dcterms:modified`) from
/// `docProps/core.xml` by local element name, parsing the ISO-8601 value.
/// Minimal substring scan — avoids pulling in a namespaced XML model. A value
/// that names its zone (Word writes `Z`) is shown in the viewer's local time,
/// as the field examples of §17.16.5.11 do; one that names none is taken as
/// wall-clock time.
pub(crate) fn extract_core_date(xml: &str, local_name: &str) -> Option<field::DateTime> {
    let raw = core_date_text(xml, local_name)?;
    match field::DateTime::parse_iso8601_instant(raw) {
        Some(millis) => Some(field::DateTime::local_at(millis, crate::local_utc_offset_minutes(millis))),
        None => field::DateTime::parse_iso8601(raw),
    }
}

fn core_date_text<'a>(xml: &'a str, local_name: &str) -> Option<&'a str> {
    // Match `:created>` or `<created>` to be namespace-agnostic.
    let needle_ns = format!(":{local_name}>");
    let open_end = xml.find(&needle_ns).map(|i| i + needle_ns.len())
        .or_else(|| xml.find(&format!("<{local_name}>")).map(|i| i + local_name.len() + 2))?;
    let rest = &xml[open_end..];
    let close = rest.find("</")?;
    Some(rest[..close].trim())
}

// ─── Numbering helpers ──────────────────────────────────────────────────────

pub(crate) fn build_numbering_lookup<R: std::io::Read + std::io::Seek>(
    doc: &mut ooxml_wml::Document<R>,
    style_ctx: &ooxml_wml::ext::StyleContext,
) -> HashMap<(i64, i64), NumberingLevelDef> {
    let mut map = HashMap::new();

    let numbering_xml = match doc.package_mut().read_part("word/numbering.xml") {
        Ok(xml) => xml,
        Err(_) => return map,
    };
    let numbering = match ooxml_wml::ext::parse_numbering(&numbering_xml) {
        Ok(n) => n,
        Err(e) => {
            crate::console_warn(&format!("numbering parse failed: {e:?}"));
            return map;
        }
    };

    let abstract_map: HashMap<i64, &ooxml_wml::types::AbstractNumbering> = numbering
        .abstract_num
        .iter()
        .map(|a| (a.abstract_num_id, a))
        .collect();

    // num_id → abstract_num_id, for the §17.9.21 numStyleLink chase below.
    let num_to_abs: HashMap<i64, i64> = numbering.num.iter()
        .map(|n| (n.num_id, n.abstract_num_id.value))
        .collect();

    // Build one `NumberingLevelDef` from a `<w:lvl>` definition (§17.9.6).
    let build_level = |level: &ooxml_wml::types::Level,
                       list: i64,
                       start_override: Option<i64>| -> NumberingLevelDef {
        let format = level.num_fmt.as_ref()
            .map(|f| f.value)
            .unwrap_or(STNumberFormat::Decimal);
        let lvl_text = level.lvl_text.as_ref()
            .and_then(|t| t.value.as_deref())
            .unwrap_or("")
            .to_string();
        let start = level.start.as_ref().map(|s| s.value).unwrap_or(1);
        // §17.9.6 lvlRestart: 1-based index of the earliest level whose use
        // restarts this level. Omitted → restart on any earlier level (None
        // here, resolved at counter time). 0 → never restart.
        let lvl_restart = level.lvl_restart.as_ref().map(|d| d.value);

        // §17.9.28 w:suff — separator between marker and text; omitted → tab.
        let suffix = level.suff.as_ref()
            .map(|s| s.value)
            .unwrap_or(ooxml_wml::types::STLevelSuffix::Tab);

        NumberingLevelDef {
            format,
            counter: crate::wml::numbering::counter_style(
                &format,
                level.num_fmt.as_ref().and_then(|f| f.format.as_deref()),
            ),
            lvl_text,
            list,
            start,
            start_override,
            p_style: level.paragraph_style.as_ref().map(|s| s.value.clone()),
            lvl_restart,
            // §17.9.22: the level's paragraph properties, applied in layout.
            p_pr: level.p_pr.as_deref().cloned().map(std::rc::Rc::new),
            legal: crate::check_toggle(&level.is_lgl).unwrap_or(false),
            jc: level.lvl_jc.as_ref().map_or_else(Default::default, |j| crate::wml::numbering::MarkerJc::from_st(j.value)),
            null_text: level.lvl_text.as_ref().and_then(|t| t.null.as_deref()).is_some_and(crate::on_off),
            suffix,
            // §17.9.24 — the level's own rPr formats the numbering text. This
            // closure receives the already-override-resolved lvl (§17.9.8
            // whole-level replacement), so an override's rPr comes free.
            rpr: level.r_pr.as_ref().map(|r| MarkerRunOverrides::from_rpr(r)).unwrap_or_default(),
        }
    };

    for inst in &numbering.num {
        let abs_id = inst.abstract_num_id.value;
        let Some(mut abs) = abstract_map.get(&abs_id).copied() else { continue };
        // §17.9.21 numStyleLink: this abstractNum is a REFERENCE to a numbering
        // style; the style's pPr/numPr/numId names the num whose abstractNum
        // (carrying the matching w:styleLink, §17.9.27) holds the real levels.
        // Word emits one hop; cap the walk so malformed cycles terminate.
        //
        // Settled semantics (floors — changing any needs a deliberate decision):
        // - ANY chase failure (missing style, no pPr/numPr, target num/abstract
        //   missing) → break → fall back to the current abstract as-is. A
        //   level-less linking abstract then produces no map entries and the
        //   synthesized-bullet default (numbering.rs) keeps a marker — the
        //   §17.9.27 degrade precedent ("references a style which does not
        //   exist → shall not be the underlying properties").
        // - An abstract carrying BOTH numStyleLink and its own lvl children:
        //   the chase wins (§17.9.21 first sentence — a reference abstract
        //   "does not contain the actual numbering properties"); its own lvls
        //   serve only as chase-failure fallback.
        // - w:styleLink (§17.9.27) gets NO code — it is the declaration side /
        //   "friendly name"; its presence must not suppress anything, and on an
        //   abstract that also carries numStyleLink, numStyleLink wins.
        // - The chase reads the named style's OWN pPr/numPr only — no
        //   basedOn-chain walk (Word writes numPr directly on numbering styles).
        // - Depth cap 8, no visited set (the cap alone terminates cycles).
        // - The target must be a NUMBERING style (§17.9.21 "a numbering style
        //   stored in the document"); a wrong-typed styleId match is ignored,
        //   mirroring §17.9.23's sibling rule for wrong-typed references.
        for _ in 0..8 {
            let Some(next) = abs.num_style_link.as_ref()
                .and_then(|l| style_ctx.style(&l.value))
                .filter(|s| s.r#type == Some(ooxml_wml::types::STStyleType::Numbering))
                .and_then(|s| s.p_pr.as_ref())
                .and_then(|p| p.num_pr.as_ref())
                .and_then(|np| np.num_id.as_deref()).map(|n| n.value)
                .and_then(|nid| num_to_abs.get(&nid))
                .and_then(|aid| abstract_map.get(aid)).copied()
            else { break };
            abs = next;
        }

        // §17.9.8/9 instance overrides keyed by ilvl: an optional whole-level
        // `<w:lvl>` replacement and/or a `<w:startOverride>` restart value
        // (§17.9.26). A replacement level's own `w:start` seeds the list's
        // sequence only when this instance is the first to use the level;
        // unlike startOverride it does not reset a running one (not measured
        // in Word).
        // FLOOR: overrides stay keyed on the REFERENCING instance and apply to
        // the (possibly chased) abstract's levels; lvlOverrides on the
        // intermediate/DEFINING num are NOT merged — Word emits bare
        // `<w:num><w:abstractNumId/></w:num>` for style-backing instances, so
        // this is untestable against the corpus.
        let mut override_lvl: HashMap<i64, &ooxml_wml::types::Level> = HashMap::new();
        let mut override_start: HashMap<i64, i64> = HashMap::new();
        for ov in &inst.lvl_override {
            let il = ov.ilvl;
            if let Some(ref l) = ov.lvl {
                override_lvl.insert(il, l.as_ref());
            }
            if let Some(ref s) = ov.start_override {
                override_start.insert(il, s.value);
            }
        }

        for level in &abs.lvl {
            // A whole-level override replaces the abstract definition (§17.9.8);
            // a bare startOverride only resets the start value (§17.9.9).
            let il = level.ilvl;
            let src = override_lvl.get(&il).copied().unwrap_or(level);
            let start_ov = override_start.get(&il).copied();
            // Counters belong to the instance's own abstract list, not the one
            // a numStyleLink chase reached (a distinct abstract id).
            map.insert((inst.num_id, il), build_level(src, abs_id, start_ov));
        }
    }

    map
}

// ─── The default paragraph style ────────────────────────────────────────────

/// Key under which the document's default paragraph style is registered in the
/// style map. NUL cannot appear in XML, so no real styleId can collide with it.
pub(crate) const DEFAULT_PARAGRAPH_STYLE_KEY: &str = "\u{0}default-paragraph-style";

/// §17.7.4.17: the default paragraph style "applies to every paragraph that
/// carries no pStyle". It is the style with `w:default` true and type paragraph
/// (an absent `w:type` means paragraph); when several claim it, the last one in
/// the part wins. Word writes Normal paragraphs without a pStyle, so most body
/// text reaches its Normal style only through this rule.
///
/// Registered once at load as an alias entry whose `basedOn` is the default
/// style, so every style-chain walk falls back to `DEFAULT_PARAGRAPH_STYLE_KEY`
/// and reaches it in one keyed lookup. Picked from `styles.style` (document
/// order), not from the context's map, which has none.
pub(crate) fn register_default_paragraph_style(
    ctx: &mut ooxml_wml::ext::StyleContext,
    styles: &ooxml_wml::types::Styles,
) {
    use ooxml_wml::types::{CTString, STStyleType, Style};
    let default_id = styles
        .style
        .iter()
        .rev()
        .filter(|s| {
            matches!(s.r#type, None | Some(STStyleType::Paragraph))
                && s.default.as_deref().is_some_and(crate::on_off)
        })
        .find_map(|s| s.style_id.clone());
    if let Some(id) = default_id {
        ctx.styles.insert(
            DEFAULT_PARAGRAPH_STYLE_KEY.to_string(),
            Style {
                based_on: Some(Box::new(CTString { value: id })),
                ..Default::default()
            },
        );
    }
}

/// The default paragraph style's id, if the document declares one.
pub(crate) fn default_paragraph_style_id(ctx: &ooxml_wml::ext::StyleContext) -> Option<&str> {
    ctx.style(DEFAULT_PARAGRAPH_STYLE_KEY)
        .and_then(|alias| alias.based_on.as_ref())
        .map(|b| b.value.as_str())
}

// ─── Built-in header/footer styles ──────────────────────────────────────────

/// WORD-BEHAVIOR (no ECMA definition — the same precedent as the default for an
/// absent compatibilityMode): real documents reference `<w:pStyle
/// w:val="Header"/>` / `"Footer"` on hdr/ftr paragraphs without defining them
/// in styles.xml; Word then applies its Normal.dotm built-ins (§17.7.4.5/.6
/// acknowledge app-side latent styles but do not define their content): tabs
/// center@4680 + right@9360 twips, spacing after=0 line=240 lineRule=auto,
/// basedOn the document's default paragraph style. The positions are FIXED in
/// the style definition — Word does NOT recompute them per page size (A4 docs
/// from a US template keep 4680/9360; A4-locale templates carry 4819/9638 —
/// no locale detection is attempted, a deliberate floor).
/// Insert-only-when-absent: a styles.xml definition always masks this.
///
/// FLOORS (deliberate): only Header/Footer are synthesized (other latent
/// built-ins — Heading1-9, TOC, ListParagraph, … — still fall back to bare
/// document defaults); matching is by exact styleId "Header"/"Footer" (Word's
/// canonical en emission — localized ids like "Kopfzeile" and name-based
/// latent aliasing are not chased); the linked character styles HeaderChar/
/// FooterChar are not synthesized (no layout effect without an rStyle ref).
pub(crate) fn inject_builtin_hdr_ftr_styles(ctx: &mut ooxml_wml::ext::StyleContext) {
    use ooxml_wml::types::{
        CTPPrGeneral, CTSpacing, CTString, CTTabStop, CTTabs, STLineSpacingRule, STStyleType,
        STTabJc, Style,
    };
    // basedOn target: the document's default paragraph style (Word chains the
    // built-ins off Normal), as `register_default_paragraph_style` picked it,
    // which must run first. A missing target breaks safely — the style-chain
    // collector tolerates dangling basedOn ids.
    let default_para_id = default_paragraph_style_id(ctx).unwrap_or("Normal").to_string();
    for (id, name) in [("Header", "header"), ("Footer", "footer")] {
        if ctx.styles.contains_key(id) {
            continue;
        }
        ctx.styles.insert(
            id.to_string(),
            Style {
                r#type: Some(STStyleType::Paragraph),
                style_id: Some(id.to_string()),
                name: Some(Box::new(CTString { value: name.to_string() })),
                based_on: Some(Box::new(CTString { value: default_para_id.clone() })),
                p_pr: Some(Box::new(CTPPrGeneral {
                    tabs: Some(Box::new(CTTabs {
                        tab: vec![
                            CTTabStop { value: STTabJc::Center, leader: None, pos: "4680".into() },
                            CTTabStop { value: STTabJc::Right, leader: None, pos: "9360".into() },
                        ],
                        ..Default::default()
                    })),
                    spacing: Some(Box::new(CTSpacing {
                        after: Some("0".into()),
                        line: Some("240".into()),
                        line_rule: Some(STLineSpacingRule::Auto),
                        ..Default::default()
                    })),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
    }
}

// ─── Section builder ───────────────────────────────────────────────────────

/// A section ends at a paragraph carrying `w:sectPr` (§17.6.18). When that
/// paragraph sits inside a block-level content control or custom XML element
/// (§17.5.2.29, §17.5.1.6), the wrapper is replaced by its child blocks so the
/// section split below can see the break. Wrappers holding no section break
/// are kept: layout unwraps them anyway.
fn unwrap_section_breaks(blocks: Vec<BlockContent>) -> Vec<BlockContent> {
    use ooxml_wml::types::BlockContentChoice as C;
    fn ends_section(p: &ooxml_wml::types::Paragraph) -> bool {
        p.p_pr.as_ref().and_then(|ppr| ppr.sect_pr.as_ref()).is_some()
    }
    fn holds_break(children: &[C]) -> bool {
        children.iter().any(|c| match c {
            C::P(p) => ends_section(p),
            C::Sdt(s) => s.sdt_content.as_ref().is_some_and(|c| holds_break(&c.block_content)),
            C::CustomXml(c) => holds_break(&c.block_content),
            _ => false,
        })
    }
    fn lift(children: Vec<C>, out: &mut Vec<BlockContent>) {
        for c in children {
            match c {
                C::P(p) => out.push(BlockContent::P(p)),
                C::Tbl(t) => out.push(BlockContent::Tbl(t)),
                C::Sdt(s) if s.sdt_content.as_ref().is_some_and(|c| holds_break(&c.block_content)) => {
                    lift(s.sdt_content.map(|c| c.block_content).unwrap_or_default(), out);
                }
                C::CustomXml(c) if holds_break(&c.block_content) => lift(c.block_content, out),
                C::Sdt(s) => out.push(BlockContent::Sdt(s)),
                C::CustomXml(c) => out.push(BlockContent::CustomXml(c)),
                _ => {}
            }
        }
    }
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            BlockContent::Sdt(s) if s.sdt_content.as_ref().is_some_and(|c| holds_break(&c.block_content)) => {
                lift(s.sdt_content.map(|c| c.block_content).unwrap_or_default(), &mut out);
            }
            BlockContent::CustomXml(c) if holds_break(&c.block_content) => lift(c.block_content, &mut out),
            other => out.push(other),
        }
    }
    out
}

/// Whether the on/off setting `name` (an element of `word/settings.xml`) is
/// on: present with no `w:val`, or `w:val` of `true`, `on` or `1`
/// (§17.17.4 `ST_OnOff`). Absent is off.
pub(crate) fn settings_on_off(settings_xml: &str, name: &str) -> bool {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(settings_xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == name.as_bytes() => {
                let val = e.attributes().flatten()
                    .find(|a| a.key.local_name().as_ref() == b"val")
                    .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                return val.as_deref().is_none_or(crate::on_off);
            }
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

pub(crate) fn build_sections<R: std::io::Read + std::io::Seek>(
    doc: &mut ooxml_wml::Document<R>,
    theme: &crate::engine::fonts::ThemeFonts,
) -> Vec<Section> {
    let even_odd = doc.package_mut().read_part_string("word/settings.xml")
        .is_ok_and(|xml| settings_on_off(&xml, "evenAndOddHeaders"));
    let doc_notes = crate::wml::notes::SectionNotes::from_settings(
        doc.package_mut().read_part("word/settings.xml").ok().as_deref(),
    );

    let body_sect_pr = doc.body().section_properties().cloned();
    let blocks = unwrap_section_breaks(std::mem::take(&mut doc.body_mut().block_content));

    let mut sections: Vec<Section> = Vec::new();
    let mut current_blocks: Vec<BlockContent> = Vec::new();

    for block in blocks {
        let is_section_end = matches!(&block, BlockContent::P(para)
            if para.p_pr.as_ref().and_then(|ppr| ppr.sect_pr.as_ref()).is_some());

        current_blocks.push(block);

        if is_section_end {
            let sect_pr = match current_blocks.last() {
                Some(BlockContent::P(para)) => para.p_pr.as_ref()
                    .and_then(|ppr| ppr.sect_pr.as_ref())
                    .map(|sp| sp.as_ref()),
                _ => None,
            };
            if let Some(sect_pr) = sect_pr {
                let geometry = PageGeometry::from_sect_pr(sect_pr, theme);
                let hdr_ftr = load_section_hdr_ftr(doc, &sect_pr.header_footer_refs, sect_pr.has_title_page(), even_odd);
                let (break_type, page_numbering) = (SectionBreakType::of(sect_pr), PageNumbering::of(sect_pr));
                let notes = doc_notes.section(sect_pr);
                sections.push(Section {
                    blocks: std::mem::take(&mut current_blocks),
                    geometry,
                    hdr_ftr,
                    break_type,
                    page_numbering,
                    notes,
                });
            }
        }
    }

    // Remaining blocks belong to the last section (body-level sectPr).
    // The body sectPr's `w:type` describes how this final section *begins*
    // relative to the previous one — so a body-level `continuous` must be
    // honored (it was previously hardcoded to NextPage, dropping that type).
    let (geometry, hdr_ftr, break_type, page_numbering, notes) = if let Some(ref sect_pr) = body_sect_pr {
        let geo = PageGeometry::from_sect_pr(sect_pr, theme);
        let hf = load_section_hdr_ftr(doc, &sect_pr.header_footer_refs, sect_pr.has_title_page(), even_odd);
        (geo, hf, SectionBreakType::of(sect_pr), PageNumbering::of(sect_pr), doc_notes.section(sect_pr))
    } else {
        (PageGeometry::us_letter(), SectionHdrFtr {
            default_header: None, first_header: None, even_header: None,
            default_footer: None, first_footer: None, even_footer: None,
            part_paths: Vec::new(),
            slot_parts: Default::default(),
            title_pg: false, even_odd,
        }, SectionBreakType::NextPage, PageNumbering::default(), doc_notes)
    };
    sections.push(Section {
        blocks: current_blocks,
        geometry,
        hdr_ftr,
        break_type,
        page_numbering,
        notes,
    });

    // §17.10.1/§17.10.2: a section that omits a header/footer TYPE inherits that
    // type from the RESOLVED previous section (per-type). Processing left→right
    // means sections[i-1] is already fully resolved, so inheritance chains across
    // 3+ sections. Section 0 never inherits (first-section blank-on-omit).
    for i in 1..sections.len() {
        let (left, right) = sections.split_at_mut(i);
        inherit_hdr_ftr(&mut right[0].hdr_ftr, &left[i - 1].hdr_ftr);
    }

    sections
}

pub(crate) fn load_section_hdr_ftr<R: std::io::Read + std::io::Seek>(
    doc: &mut ooxml_wml::Document<R>,
    refs: &[HeaderFooterRef],
    title_pg: bool,
    even_odd: bool,
) -> SectionHdrFtr {
    let mut hf = SectionHdrFtr {
        default_header: None, first_header: None, even_header: None,
        default_footer: None, first_footer: None, even_footer: None,
        part_paths: Vec::new(),
        slot_parts: Default::default(),
        title_pg, even_odd,
    };

    for r in refs {
        let hf_rel_id = match r {
            HeaderFooterRef::HeaderReference(h) => &h.id,
            HeaderFooterRef::FooterReference(f) => &f.id,
        };
        let part = doc.doc_relationships().get(hf_rel_id).map(|rel| {
            if rel.target.starts_with('/') {
                rel.target.trim_start_matches('/').to_string()
            } else {
                format!("word/{}", rel.target.trim_start_matches("word/").trim_start_matches('/'))
            }
        });
        if let Some(path) = &part {
            if !hf.part_paths.contains(path) {
                hf.part_paths.push(path.clone());
            }
        }
        let slot = match r {
            HeaderFooterRef::HeaderReference(h) => match h.r#type { STHdrFtr::Default => 0, STHdrFtr::First => 1, STHdrFtr::Even => 2 },
            HeaderFooterRef::FooterReference(f) => match f.r#type { STHdrFtr::Default => 3, STHdrFtr::First => 4, STHdrFtr::Even => 5 },
        };
        hf.slot_parts[slot] = part;

        match r {
            HeaderFooterRef::HeaderReference(href) => {
                let content = doc.get_header(&href.id)
                    .ok()
                    .map(|h| h.block_content);
                match href.r#type {
                    STHdrFtr::Default => hf.default_header = content,
                    STHdrFtr::First => hf.first_header = content,
                    STHdrFtr::Even => hf.even_header = content,
                }
            }
            HeaderFooterRef::FooterReference(fref) => {
                let content = doc.get_footer(&fref.id)
                    .ok()
                    .map(|f| f.block_content);
                match fref.r#type {
                    STHdrFtr::Default => hf.default_footer = content,
                    STHdrFtr::First => hf.first_footer = content,
                    STHdrFtr::Even => hf.even_footer = content,
                }
            }
        }
    }

    hf
}

/// Fill each of the 6 hdr/ftr slots this section did NOT define from the
/// previous (already-resolved) section, per-type (§17.10.1/§17.10.2). Only when
/// at least one slot is actually inherited do we union the previous section's
/// `part_paths` (deduped) so the CLONED blocks' image rels still resolve in
/// extract_hdr_ftr_images. title_pg (per-section, from this section's own flag)
/// and even_odd (doc-global) are left untouched.
fn inherit_hdr_ftr(cur: &mut SectionHdrFtr, prev: &SectionHdrFtr) {
    let mut inherited = false;
    macro_rules! carry { ($slot:ident, $i:expr) => {
        if cur.$slot.is_none() {
            if let Some(b) = &prev.$slot {
                cur.$slot = Some(b.clone());
                cur.slot_parts[$i] = prev.slot_parts[$i].clone();
                inherited = true;
            }
        }
    }; }
    carry!(default_header, 0); carry!(first_header, 1); carry!(even_header, 2);
    carry!(default_footer, 3); carry!(first_footer, 4); carry!(even_footer, 5);
    if inherited {
        for p in &prev.part_paths {
            if !cur.part_paths.contains(p) { cur.part_paths.push(p.clone()); }
        }
    }
}

pub(crate) fn extract_hdr_ftr_images<R: std::io::Read + std::io::Seek>(
    doc: &mut ooxml_wml::Document<R>,
    hdr_ftr: &mut SectionHdrFtr,
    images: &mut HashMap<String, ImageBytes>,
    charts: &mut HashMap<String, Vec<u8>>,
) {
    // Each part's image and chart relationships, namespaced as
    // `<part>::<rId>` so two parts that both use `rId1` stay apart. A chart
    // part's XML is stored under `chart::<part>::<rId>`, the key its drawing
    // resolves to once rewritten.
    let mut remaps: HashMap<String, HashMap<String, String>> = HashMap::new();
    for part_path in hdr_ftr.part_paths.clone() {
        let part_rels = match doc.package_mut().read_part_relationships(&part_path) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let mut remap: HashMap<String, String> = HashMap::new();

        for rel in part_rels.iter() {
            let target_path = || if rel.target.starts_with('/') {
                rel.target.trim_start_matches('/').to_string()
            } else if let Some(slash) = part_path.rfind('/') {
                format!("{}/{}", &part_path[..slash], &rel.target)
            } else {
                rel.target.clone()
            };
            if rel.relationship_type.ends_with("/chart") {
                let namespaced_id = format!("{}::{}", part_path, rel.id);
                remap.insert(rel.id.clone(), namespaced_id.clone());
                if let Ok(xml) = doc.package_mut().read_part(&target_path()) {
                    charts.insert(format!("chart::{namespaced_id}"), xml);
                }
                continue;
            }
            if !rel.relationship_type.contains("/image") { continue; }

            let namespaced_id = format!("{}::{}", part_path, rel.id);
            remap.insert(rel.id.clone(), namespaced_id.clone());

            if images.contains_key(&namespaced_id) { continue; }

            let image_path = target_path();

            match doc.package_mut().read_part(&image_path) {
                Ok(data) => {
                    let ext = image_path.rsplit('.').next().unwrap_or("png");
                    let mime = match ext {
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "bmp" => "image/bmp",
                        "tiff" | "tif" => "image/tiff",
                        "svg" => "image/svg+xml",
                        "emf" => "image/x-emf",
                        "wmf" => "image/x-wmf",
                        _ => "image/png",
                    };
                    images.insert(namespaced_id.clone(), ImageBytes {
                        mime: mime.to_string(),
                        data,
                    });
                }
                Err(_) => continue,
            }
        }
        remaps.insert(part_path, remap);
    }

    // Rewrite each slot with its own part's map, at any depth (tables,
    // content controls, hyperlinks), not every slot with every part's.
    let slots = [
        &mut hdr_ftr.default_header, &mut hdr_ftr.first_header, &mut hdr_ftr.even_header,
        &mut hdr_ftr.default_footer, &mut hdr_ftr.first_footer, &mut hdr_ftr.even_footer,
    ];
    for (blocks, part) in slots.into_iter().zip(hdr_ftr.slot_parts.iter()) {
        let (Some(blocks), Some(remap)) = (blocks.as_mut(), part.as_ref().and_then(|p| remaps.get(p))) else { continue };
        if remap.is_empty() { continue; }
        crate::wml::content_walk::for_each_run_mut(blocks, &mut |run| {
            for rc in &mut run.run_content {
                if let ooxml_wml::types::RunContent::Drawing(d) = rc {
                    for child in &mut d.extra_children {
                        if let ooxml_xml::RawXmlNode::Element(ref mut elem) = child.node {
                            rewrite_blip_embed_ids(elem, remap);
                        }
                    }
                }
            }
        });
    }
}

/// Rewrite a drawing's relationship references through `remap`: a picture's
/// `a:blip@r:embed` and a chart's `c:chart@r:id` (§21.2.2.27).
pub(crate) fn rewrite_blip_embed_ids(elem: &mut ooxml_xml::RawXmlElement, remap: &HashMap<String, String>) {
    let local = elem.name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&elem.name);
    let reference = match local {
        "blip" => Some("embed"),
        "chart" => Some("id"),
        _ => None,
    };
    if let Some(reference) = reference {
        for (attr_name, attr_val) in &mut elem.attributes {
            let attr_local = attr_name.rsplit_once(':').map(|(_, l)| l).unwrap_or(attr_name);
            if attr_local == reference {
                if let Some(new_id) = remap.get(attr_val.as_str()) {
                    *attr_val = new_id.clone();
                }
            }
        }
    }
    for child in &mut elem.children {
        if let ooxml_xml::RawXmlNode::Element(ref mut child_elem) = child {
            rewrite_blip_embed_ids(child_elem, remap);
        }
    }
}

#[cfg(test)]
mod inherit_tests {
    use super::*;
    use ooxml_wml::ext::ParagraphExt;
    use std::io::{Cursor, Write};

    // Two sections: section 1 defines a default header+footer via a
    // paragraph-embedded sectPr; section 2 (the body-level sectPr) declares ONLY
    // pgSz+pgMar — no header/footerReference. Per §17.10.1/§17.10.2 section 2
    // inherits section 1's default header+footer.
    fn two_section_omitted_ref_docx() -> Vec<u8> {
        const CT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
  <Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>
</Types>"#;
        const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>
    <w:p><w:pPr><w:sectPr>
      <w:headerReference w:type="default" r:id="rId2"/>
      <w:footerReference w:type="default" r:id="rId3"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
    </w:sectPr></w:pPr><w:r><w:t>Section one page.</w:t></w:r></w:p>
    <w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>Section two page — no refs.</w:t></w:r></w:p>
    <w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
    </w:sectPr>
  </w:body>
</w:document>"#;
        const DOC_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
</Relationships>"#;
        const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:pPr><w:jc w:val="right"/></w:pPr><w:r><w:rPr><w:sz w:val="18"/></w:rPr><w:t>Inherited Header</w:t></w:r></w:p></w:hdr>"#;
        const FOOTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:rPr><w:sz w:val="18"/></w:rPr><w:t>Inherited Footer</w:t></w:r></w:p></w:ftr>"#;
        let parts: &[(&str, &str)] = &[
            ("[Content_Types].xml", CT),
            ("_rels/.rels", ROOT_RELS),
            ("word/document.xml", DOC),
            ("word/_rels/document.xml.rels", DOC_RELS),
            ("word/header1.xml", HEADER),
            ("word/footer1.xml", FOOTER),
        ];
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, content) in parts {
            zw.start_file(*name, opts).expect("zip start_file");
            zw.write_all(content.as_bytes()).expect("zip write");
        }
        zw.finish().expect("zip finish").into_inner()
    }

    // §17.10.1/§17.10.2 — section 2 omits ALL hdr/ftr refs; post-fix it inherits
    // section 1's default header+footer (per-type), and section 1's part_paths are
    // unioned in so the cloned blocks' rels still resolve. Pre-fix (no post-pass)
    // section 2's slots stay None → the first `is_some()` assert panics.
    #[test]
    fn section_omitting_refs_inherits_previous_hdr_ftr() {
        let mut doc =
            ooxml_wml::Document::from_reader(Cursor::new(two_section_omitted_ref_docx())).expect("parse");
        let sections = build_sections(&mut doc, &crate::engine::fonts::ThemeFonts::empty());
        assert_eq!(sections.len(), 2, "two sections expected");

        let hf1 = &sections[1].hdr_ftr;
        assert!(hf1.default_header.is_some(), "section 2 must INHERIT section 1's default header");
        assert!(hf1.default_footer.is_some(), "section 2 must INHERIT section 1's default footer");

        // Prove the inherited content is section 1's, via ParagraphExt::text().
        let hdr_text: String = hf1.default_header.as_ref().unwrap().iter()
            .filter_map(|b| match b { BlockContent::P(p) => Some(p.text()), _ => None })
            .collect();
        assert!(hdr_text.contains("Inherited Header"), "inherited header text mismatch: {hdr_text:?}");

        // part_paths must be unioned so the inherited blocks' rels resolve.
        assert!(hf1.part_paths.iter().any(|p| p.contains("header1.xml")),
            "inherited part_paths must include header1.xml: {:?}", hf1.part_paths);

        // Section 1 keeps its OWN refs (unchanged).
        assert!(sections[0].hdr_ftr.default_header.is_some());
    }
}
