//! In-memory zip rebuild: replace ONE part's bytes, copy every other entry.
//!
//! The WML/PML rules target an interior package part (`word/document.xml`,
//! `ppt/slides/slideN.xml`), so applying a repair means producing new `.docx` /
//! `.pptx` bytes with only that one part rewritten and every other entry — media,
//! rels, styles, content types — preserved exactly. We re-open the original
//! bytes as a `ZipArchive`, stream each entry through, and substitute the bytes
//! of the named part(s). All entries are re-emitted `Stored` (uncompressed): the
//! result is immediately re-parsed in-process and discarded, so wire size is
//! irrelevant and `Stored` avoids any re-deflate cost. Lenient fork codegen deletes this
//! with the module.

use std::io::{Cursor, Read, Write};

/// Rebuild `original` zip bytes, replacing the bytes of each part named in
/// `replacements` (`part_path -> new_bytes`) and copying every other entry
/// unchanged. Part paths are matched against the archive entry names verbatim
/// (OPC part paths are the zip entry names, e.g. `word/document.xml`). Directory
/// entries are skipped (OPC packages carry none meaningfully). Returns `None` on
/// any zip read/write error (the caller then keeps the original-bytes failure —
/// graceful, never worse than not repairing).
pub(crate) fn rebuild_zip_replacing_part(
    original: &[u8],
    replacements: &[(String, Vec<u8>)],
) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(original)).ok()?;
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().ok()?.into_owned();
        out.start_file(name.clone(), opts).ok()?;
        if let Some((_, bytes)) = replacements.iter().find(|(p, _)| *p == name) {
            out.write_all(bytes).ok()?;
        } else {
            let declared = entry.size();
            out.write_all(&read_entry_bytes(&mut entry, declared)?).ok()?;
        }
    }

    Some(out.finish().ok()?.into_inner())
}

/// Largest buffer pre-reserved from a zip entry's DECLARED size. The declared
/// size comes from the central directory, which the file's author controls: a
/// 200-byte package can claim an entry of 2^62 bytes, and reserving that aborts
/// the process before a single byte is read. Reads still grow past this as real
/// data arrives; the cap only bounds what is trusted up front.
const PREALLOC_CAP: u64 = 16 * 1024 * 1024;

/// Read a zip entry to the end. `declared` is the entry's `size()`, used only as
/// a capped capacity hint (see [`PREALLOC_CAP`]). `None` if the read fails.
pub(crate) fn read_entry_bytes(mut entry: impl Read, declared: u64) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(declared.min(PREALLOC_CAP) as usize);
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}
