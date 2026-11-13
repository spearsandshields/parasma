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
use crate::wml::section::{PageGeometry, SectionBreakType};

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
    pub(crate) title_pg: bool,
    pub(crate) even_odd: bool,
}

pub(crate) struct Section {
    pub(crate) blocks: Vec<BlockContent>,
    pub(crate) geometry: PageGeometry,
    pub(crate) hdr_ftr: SectionHdrFtr,
    pub(crate) break_type: SectionBreakType,
}

/// Extract a core-property date (`dcterms:created`/`dcterms:modified`) from
/// `docProps/core.xml` by local element name, parsing the ISO-8601 value.
/// Minimal substring scan — avoids pulling in a namespaced XML model.
pub(crate) fn extract_core_date(xml: &str, local_name: &str) -> Option<field::DateTime> {
    // Match `:created>` or `<created>` to be namespace-agnostic.
    let needle_ns = format!(":{local_name}>");
    let open_end = xml.find(&needle_ns).map(|i| i + needle_ns.len())
        .or_else(|| xml.find(&format!("<{local_name}>")).map(|i| i + local_name.len() + 2))?;
    let rest = &xml[open_end..];
    let close = rest.find("</")?;
    field::DateTime::parse_iso8601(rest[..close].trim())
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
                       start_override: Option<i64>| -> NumberingLevelDef {
        let format = level.num_fmt.as_ref()
            .map(|f| f.value)
            .unwrap_or(STNumberFormat::Decimal);
        let lvl_text = level.lvl_text.as_ref()
            .and_then(|t| t.value.as_deref())
            .unwrap_or("")
            .to_string();
        // §17.9.9 startOverride replaces the abstract level's w:start value.
        let start = start_override
            .or_else(|| level.start.as_ref().map(|s| s.value))
            .unwrap_or(1);
        // §17.9.6 lvlRestart: 1-based index of the earliest level whose use
        // restarts this level. Omitted → restart on any earlier level (None
        // here, resolved at counter time). 0 → never restart.
        let lvl_restart = level.lvl_restart.as_ref().map(|d| d.value);

        let (indent_left, indent_hanging) = level.p_pr.as_ref()
            .and_then(|ppr| ppr.indentation.as_ref())
            .map(|ind| {
                let left = ind.left.as_deref()
                    .or(ind.start.as_deref())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let hanging = ind.hanging.as_deref()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                (left, hanging)
            })
            .unwrap_or(((level.ilvl + 1) * 720, 360));

        // §17.9.28 w:suff — separator between marker and text; omitted → tab.
        let suffix = level.suff.as_ref()
            .map(|s| s.value)
            .unwrap_or(ooxml_wml::types::STLevelSuffix::Tab);

        NumberingLevelDef {
            format,
            lvl_text,
            start,
            lvl_restart,
            indent_left_twips: indent_left,
            indent_hanging_twips: indent_hanging,
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
        // `<w:lvl>` replacement and/or a `<w:startOverride>` start value.
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
            map.insert((inst.num_id, il), build_level(src, start_ov));
        }
    }

    map
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
    // built-ins off Normal). A missing target breaks safely — the style-chain
    // collector tolerates dangling basedOn ids.
    //
    // Deterministic pick: `styles` is a HashMap (random iteration order), and a
    // malformed styles.xml can carry MULTIPLE w:default="1" paragraph styles —
    // `find` would make the basedOn parent vary per process (the determinism
    // hazard class the layout restartability self-tests hunt). Smallest
    // styleId wins, stably.
    let default_para_id = ctx
        .styles
        .values()
        .filter(|s| {
            matches!(s.r#type, Some(STStyleType::Paragraph))
                && s.default
                    .as_deref()
                    .is_some_and(|v| matches!(v, "1" | "true" | "on"))
        })
        .min_by(|a, b| a.style_id.cmp(&b.style_id))
        .and_then(|s| s.style_id.clone())
        .unwrap_or_else(|| "Normal".into());
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

pub(crate) fn build_sections<R: std::io::Read + std::io::Seek>(
    doc: &mut ooxml_wml::Document<R>,
) -> Vec<Section> {
    let even_odd = doc.package_mut().read_part("word/settings.xml")
        .is_ok_and(|xml| {
            std::str::from_utf8(&xml).is_ok_and(|s| s.contains("evenAndOddHeaders"))
        });

    let body_sect_pr = doc.body().section_properties().cloned();
    let blocks = std::mem::take(&mut doc.body_mut().block_content);

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
                let geometry = PageGeometry::from_sect_pr(sect_pr);
                let break_type = sect_pr.r#type.as_ref()
                    .and_then(|t| t.value.as_ref())
                    .map(|v| match v {
                        ooxml_wml::types::STSectionMark::Continuous => SectionBreakType::Continuous,
                        ooxml_wml::types::STSectionMark::EvenPage => SectionBreakType::EvenPage,
                        ooxml_wml::types::STSectionMark::OddPage => SectionBreakType::OddPage,
                        _ => SectionBreakType::NextPage,
                    })
                    .unwrap_or(SectionBreakType::NextPage);
                let hdr_ftr = load_section_hdr_ftr(doc, &sect_pr.header_footer_refs, sect_pr.has_title_page(), even_odd);
                sections.push(Section {
                    blocks: std::mem::take(&mut current_blocks),
                    geometry,
                    hdr_ftr,
                    break_type,
                });
            }
        }
    }

    // Remaining blocks belong to the last section (body-level sectPr).
    // The body sectPr's `w:type` describes how this final section *begins*
    // relative to the previous one — so a body-level `continuous` must be
    // honored (it was previously hardcoded to NextPage, dropping that type).
    let (geometry, hdr_ftr, break_type) = if let Some(ref sect_pr) = body_sect_pr {
        let geo = PageGeometry::from_sect_pr(sect_pr);
        let hf = load_section_hdr_ftr(doc, &sect_pr.header_footer_refs, sect_pr.has_title_page(), even_odd);
        let bt = sect_pr.r#type.as_ref()
            .and_then(|t| t.value.as_ref())
            .map(|v| match v {
                ooxml_wml::types::STSectionMark::Continuous => SectionBreakType::Continuous,
                ooxml_wml::types::STSectionMark::EvenPage => SectionBreakType::EvenPage,
                ooxml_wml::types::STSectionMark::OddPage => SectionBreakType::OddPage,
                _ => SectionBreakType::NextPage,
            })
            .unwrap_or(SectionBreakType::NextPage);
        (geo, hf, bt)
    } else {
        (PageGeometry::us_letter(), SectionHdrFtr {
            default_header: None, first_header: None, even_header: None,
            default_footer: None, first_footer: None, even_footer: None,
            part_paths: Vec::new(),
            title_pg: false, even_odd,
        }, SectionBreakType::NextPage)
    };
    sections.push(Section {
        blocks: current_blocks,
        geometry,
        hdr_ftr,
        break_type,
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
        title_pg, even_odd,
    };

    for r in refs {
        let hf_rel_id = match r {
            HeaderFooterRef::HeaderReference(h) => &h.id,
            HeaderFooterRef::FooterReference(f) => &f.id,
        };
        if let Some(rel) = doc.doc_relationships().get(hf_rel_id) {
            let path = if rel.target.starts_with('/') {
                rel.target.trim_start_matches('/').to_string()
            } else {
                format!("word/{}", rel.target.trim_start_matches("word/").trim_start_matches('/'))
            };
            if !hf.part_paths.contains(&path) {
                hf.part_paths.push(path);
            }
        }

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
    macro_rules! carry { ($slot:ident) => {
        if cur.$slot.is_none() {
            if let Some(b) = &prev.$slot { cur.$slot = Some(b.clone()); inherited = true; }
        }
    }; }
    carry!(default_header); carry!(first_header); carry!(even_header);
    carry!(default_footer); carry!(first_footer); carry!(even_footer);
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
) {
    let part_paths = hdr_ftr.part_paths.clone();
    for part_path in &part_paths {
        let part_rels = match doc.package_mut().read_part_relationships(part_path) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let mut remap: HashMap<String, String> = HashMap::new();

        for rel in part_rels.iter() {
            if !rel.relationship_type.contains("/image") { continue; }

            let namespaced_id = format!("{}::{}", part_path, rel.id);
            remap.insert(rel.id.clone(), namespaced_id.clone());

            if images.contains_key(&namespaced_id) { continue; }

            let image_path = if rel.target.starts_with('/') {
                rel.target.trim_start_matches('/').to_string()
            } else if let Some(slash) = part_path.rfind('/') {
                format!("{}/{}", &part_path[..slash], &rel.target)
            } else {
                rel.target.clone()
            };

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

        if remap.is_empty() { continue; }

        for blocks in [
            &mut hdr_ftr.default_header, &mut hdr_ftr.first_header,
            &mut hdr_ftr.even_header, &mut hdr_ftr.default_footer,
            &mut hdr_ftr.first_footer, &mut hdr_ftr.even_footer,
        ].into_iter().flatten() {
            rewrite_image_rel_ids(blocks, &remap);
        }
    }
}

pub(crate) fn rewrite_image_rel_ids(blocks: &mut [BlockContent], remap: &HashMap<String, String>) {
    for block in blocks {
        if let BlockContent::P(p) = block {
            for content in &mut p.paragraph_content {
                if let ooxml_wml::types::ParagraphContent::R(run) = content {
                    for rc in &mut run.run_content {
                        if let ooxml_wml::types::RunContent::Drawing(d) = rc {
                            for child in &mut d.extra_children {
                                if let ooxml_xml::RawXmlNode::Element(ref mut elem) = child.node {
                                    rewrite_blip_embed_ids(elem, remap);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn rewrite_blip_embed_ids(elem: &mut ooxml_xml::RawXmlElement, remap: &HashMap<String, String>) {
    let local = elem.name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&elem.name);
    if local == "blip" {
        for (attr_name, attr_val) in &mut elem.attributes {
            let attr_local = attr_name.rsplit_once(':').map(|(_, l)| l).unwrap_or(attr_name);
            if attr_local == "embed" {
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
        let sections = build_sections(&mut doc);
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
