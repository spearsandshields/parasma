//! Shared OPC package layer.
//!
//! OOXML containers are OPC packages — ZIP archives with `[Content_Types].xml`
//! and `_rels/*.rels` relationship parts (ECMA-376 Part 2; this module cites
//! no Part 2 §-numbers). Before this layer existed, every self-parse site
//! opened its OWN fork `Package` over a fresh
//! `bytes.to_vec()` copy — 11 sites, so a pptx load paid 7+N central-directory
//! + content-types parses (N = slide count, via the per-slide diagram walk) and
//! re-parsed one slide's `.rels` up to once per picture/chart spec.
//!
//! [`OpcPackage`] replaces that: ONE archive open per byte image (zero byte
//! copies — the cursor borrows the caller's bytes) and each `.rels` part parsed
//! at most once (an `Rc`-handed cache keyed by source part path). PML holds up
//! to TWO byte images per document — the EFFECTIVE bytes (original or RULE-3
//! custGeom-repaired) and the ORIGINAL pre-repair bytes that the custGeom
//! geometry scan needs — so the handle is per-image by construction, never per-document
//! (and never global). When no repair fired the images are identical and one
//! handle serves both.
//!
//! Error posture: every consumer degrades gracefully and independently, so the
//! whole surface is `Option`-shaped — `open` failure yields `None` and each
//! call site keeps its own empty-value fallback; a part/rels miss returns
//! `None`/empty without poisoning the cached package for other callers.
//! Conventional-path fallbacks (`xl/workbook.xml`, `ppt/tableStyles.xml`, …)
//! stay AT the call sites: they are format policy, not OPC.
//!
//! The `repair/` module deliberately does NOT use this layer: repairs are
//! byte-level zip rewrites on the failure path (plus the eager RULE 7), need
//! write access, and the module is to be deleted once the fork codegen is lenient
//! (see `repair/mod.rs`) — coupling a permanent layer to a temporary one would
//! invert that. Packages are only opened AFTER the tolerant loaders settle the
//! effective bytes; never cache one across a repair boundary.

use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;

use ooxml_opc::{Package, Relationships};

/// The fork's shared relationship-type URIs (`OFFICE_DOCUMENT`, `THEME`,
/// `CHART`, …) — re-exported so the format modules stop re-spelling them.
pub(crate) use ooxml_opc::rel_type;

#[cfg(test)]
thread_local! {
    /// Archive opens performed on THIS thread — the O(1)-opens pin.
    /// Thread-local (not a process atomic) so parallel tests never see each
    /// other's opens; a load runs on one thread.
    pub(crate) static OPEN_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// One opened OPC package over a BORROWED byte image: the fork `Package`
/// (central directory + content types, parsed once at [`OpcPackage::open`])
/// plus a parse-once `.rels` cache. Construct one per byte image at the top of
/// a load path and thread `&mut` (or `Option<&mut>`, matching each site's
/// open-failure posture) through the self-parse passes.
pub(crate) struct OpcPackage<'a> {
    pkg: Package<Cursor<&'a [u8]>>,
    /// Parsed relationships keyed by SOURCE part path (`""` = package root).
    /// `Rc` so callers hold the collection while continuing to read parts
    /// through `&mut self`; `None` records a rels-part PARSE failure (an
    /// ABSENT `.rels` part is `Some(empty)` — the fork's own distinction).
    rels: HashMap<String, Option<Rc<Relationships>>>,
}

impl<'a> OpcPackage<'a> {
    /// Open the archive: ONE central-directory + `[Content_Types].xml` parse,
    /// no byte copy. `None` on a corrupt/non-zip image (callers keep their
    /// per-site empty-value fallbacks).
    pub(crate) fn open(bytes: &'a [u8]) -> Option<Self> {
        #[cfg(test)]
        OPEN_COUNT.with(|c| c.set(c.get() + 1));
        let pkg = Package::open(Cursor::new(bytes)).ok()?;
        Some(Self { pkg, rels: HashMap::new() })
    }

    /// Inflate a part's bytes. Lazy per call (deliberately uncached: caching
    /// inflated parts trades wasm RSS — the measured ceiling — for speed and
    /// is a follow-on with measurements, not a default).
    pub(crate) fn read_part(&mut self, path: &str) -> Option<Vec<u8>> {
        self.pkg.read_part(path).ok()
    }

    /// Inflate a part as UTF-8. `None` on a missing part or invalid UTF-8.
    pub(crate) fn read_part_string(&mut self, path: &str) -> Option<String> {
        self.pkg.read_part_string(path).ok()
    }

    /// The relationships of `part_path` (`""` = the package root `.rels`),
    /// parsed AT MOST ONCE per package. An absent `.rels` part or a rels parse
    /// error both yield the empty collection — every consumer but one follows
    /// the read with a lookup, for which "no rels" and "unreadable rels" were
    /// already indistinguishable at all the earlier per-site parses.
    pub(crate) fn rels(&mut self, part_path: &str) -> Rc<Relationships> {
        self.try_rels(part_path).unwrap_or_default()
    }

    /// Like [`Self::rels`] but a rels-part PARSE failure surfaces as `None`
    /// (an absent `.rels` part is still `Some(empty)` — the fork's
    /// `read_part_relationships` posture). The one consumer that needs the
    /// distinction is `pml::parse_opc_geometry`, which has always bailed to
    /// full defaults on a garbled presentation `.rels` while proceeding on an
    /// absent one.
    pub(crate) fn try_rels(&mut self, part_path: &str) -> Option<Rc<Relationships>> {
        if let Some(cached) = self.rels.get(part_path) {
            return cached.clone();
        }
        let parsed = self.pkg.read_part_relationships(part_path).ok().map(Rc::new);
        self.rels.insert(part_path.to_string(), parsed.clone());
        parsed
    }

    /// The RAW target of the package's officeDocument relationship (root rels
    /// → `rel_type::OFFICE_DOCUMENT`) — the walk five sites re-spelled. Raw
    /// because the sites disagree on target hygiene (SML trims a leading `/`,
    /// PML uses the target verbatim) and that policy stays with them.
    pub(crate) fn office_document_target(&mut self) -> Option<String> {
        self.rels("").get_by_type(rel_type::OFFICE_DOCUMENT).map(|r| r.target.clone())
    }
}

/// Resolve a relationship target against its source part's path: absolute
/// (`/xl/media/1.png`) → leading slash stripped verbatim; relative →
/// joined to the source part's directory with `.`/`..`/empty segments
/// normalized. The ONE spelling of the rule the sml and pml modules previously
/// carried as byte-identical private duplicates.
///
/// DEVIATION (declared): the fork's `ooxml_opc::resolve_part_path` ALSO
/// normalizes `.`/`..` inside the absolute form; this keeps the earlier
/// absolute passthrough so the migration stays byte-identical. Folding onto
/// the fork's rule is a separate, tested change if ever wanted.
pub(crate) fn resolve_target(base: &str, target: &str) -> String {
    if let Some(stripped) = target.strip_prefix('/') {
        return stripped.to_string();
    }
    let base_dir = base.rfind('/').map(|i| &base[..i + 1]).unwrap_or("");
    let combined = format!("{base_dir}{target}");
    let mut parts: Vec<&str> = Vec::new();
    for seg in combined.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            _ => parts.push(seg),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny in-memory package: a root `.rels`, one part with its own
    /// `.rels`, and one media part.
    fn tiny_pkg() -> Vec<u8> {
        use std::io::Write;
        let mut cur = Cursor::new(Vec::new());
        {
            let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut w = zip::ZipWriter::new(&mut cur);
            let parts: &[(&str, &str)] = &[
                (
                    "[Content_Types].xml",
                    r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>"#,
                ),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
                ),
                ("xl/workbook.xml", "<workbook/>"),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://example.test/media" Target="media/i.bin"/></Relationships>"#,
                ),
                ("xl/media/i.bin", "media-bytes"),
            ];
            for (name, body) in parts {
                w.start_file(*name, opts).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        cur.into_inner()
    }

    #[test]
    fn opens_once_reads_parts_and_caches_rels() {
        let bytes = tiny_pkg();
        let before = OPEN_COUNT.with(|c| c.get());
        let mut pkg = OpcPackage::open(&bytes).expect("valid package");
        assert_eq!(OPEN_COUNT.with(|c| c.get()) - before, 1);

        // Root rels walk + the shared officeDocument helper agree.
        assert_eq!(pkg.office_document_target().as_deref(), Some("xl/workbook.xml"));

        // Part rels parse once and are cache-shared thereafter.
        let a = pkg.rels("xl/workbook.xml");
        let b = pkg.rels("xl/workbook.xml");
        assert!(Rc::ptr_eq(&a, &b), "second lookup must hit the cache");
        let rel = a.get("rId9").expect("rId9 present");
        let path = resolve_target("xl/workbook.xml", &rel.target);
        assert_eq!(path, "xl/media/i.bin");
        assert_eq!(pkg.read_part_string(&path).as_deref(), Some("media-bytes"));

        // A miss is None and does not poison later reads.
        assert!(pkg.read_part("no/such/part").is_none());
        assert_eq!(pkg.read_part_string("xl/workbook.xml").as_deref(), Some("<workbook/>"));

        // A part with NO .rels yields the empty collection, cached.
        assert!(pkg.rels("xl/media/i.bin").is_empty());

        // No further archive opens happened during any of the above.
        assert_eq!(OPEN_COUNT.with(|c| c.get()) - before, 1);
    }

    #[test]
    fn open_on_garbage_is_none() {
        assert!(OpcPackage::open(b"not a zip").is_none());
    }

    /// Pins the exact original semantics of the (formerly duplicated)
    /// sml/pml `resolve_path`, including the declared absolute-passthrough
    /// deviation from the fork's `resolve_part_path`.
    #[test]
    fn resolve_target_matches_the_retired_duplicates() {
        assert_eq!(resolve_target("", "xl/workbook.xml"), "xl/workbook.xml");
        assert_eq!(resolve_target("xl/workbook.xml", "sharedStrings.xml"), "xl/sharedStrings.xml");
        assert_eq!(
            resolve_target("ppt/slides/slide1.xml", "../media/image1.png"),
            "ppt/media/image1.png"
        );
        assert_eq!(resolve_target("xl/workbook.xml", "./theme//theme1.xml"), "xl/theme/theme1.xml");
        // Absolute form: slash stripped, remainder passed through VERBATIM
        // (the deviation from the fork's normalizing resolver — see the doc).
        assert_eq!(resolve_target("xl/workbook.xml", "/xl/media/i.png"), "xl/media/i.png");
        assert_eq!(resolve_target("", "/a/../b.xml"), "a/../b.xml");
    }
}
