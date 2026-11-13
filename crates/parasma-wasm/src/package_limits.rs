//! Decompression-bomb guard: refuse a package whose parts inflate far beyond
//! what the file's size can justify, BEFORE any parser touches it.
//!
//! An OOXML package is a zip (ECMA-376 Part 2, Open Packaging Conventions), so a 255 KB `.docx` can
//! carry 100 MB of `word/document.xml`. Every later stage is linear in the
//! inflated size, so nothing downstream misbehaves — it is simply handed minutes
//! of work and hundreds of megabytes by a file that looked tiny. Measured on a
//! release build: 100 MB of text in a 255 KB package laid out 23,778 pages in
//! 217 s. The engine's memory budget (`setMemoryBudget`) stops layout, but only
//! after the whole part has been inflated and parsed.
//!
//! The rule is a compression-ratio limit: an entry that inflates to more than
//! [`MAX_RATIO`] times its compressed size is refused. Two refinements, both
//! deliberate:
//!
//! * A ratio judged from the first ~100 KiB of output gives false positives on
//!   real, repetitive spreadsheets. Here the ratio
//!   is only judged once an entry passes [`RATIO_FLOOR`]; below that, any ratio
//!   is cheap enough to accept.
//! * The counts are of bytes ACTUALLY inflated, never the sizes the central
//!   directory declares. A forged directory cannot hide a bomb: the `zip`
//!   crate refuses to inflate past an entry's declared size, so that entry
//!   fails to read and is left to the parser. Metering the real stream stops at
//!   the limit, so a refused bomb costs at most [`RATIO_FLOOR`] or the ratio budget of work.
//!
//! Cost on an ordinary document is one extra inflate pass over the package.
//! A package that is not a readable zip passes untouched: the format parser
//! reports its own, more specific error.

use std::fmt;
use std::io::{self, Cursor, Write};

/// Largest accepted inflated-to-compressed ratio for an entry past
/// [`RATIO_FLOOR`]. Real XML parts compress roughly 5–30×, so 100 leaves margin.
const MAX_RATIO: u64 = 100;

/// Inflated size below which an entry's ratio is not judged.
const RATIO_FLOOR: u64 = 4 * 1024 * 1024;

/// Ceiling on the inflated size of the whole package. Stays well inside the
/// ~1.5 GiB transient headroom the memory ledger reserves above the layout
/// budget (`docs/memory-architecture.md`, "The budget-headroom invariant").
const MAX_TOTAL: u64 = 1024 * 1024 * 1024;

/// Why a package was refused. The `Display` text is the load error the
/// integrator sees.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PackageLimit {
    /// One part inflates past `MAX_RATIO` × its compressed size.
    Ratio { part: String },
    /// All parts together inflate past `MAX_TOTAL`.
    Total,
}

impl fmt::Display for PackageLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageLimit::Ratio { part } => write!(
                f,
                "package refused: part `{part}` expands to more than {MAX_RATIO}x its compressed size \
                 (decompression-bomb guard)"
            ),
            PackageLimit::Total => write!(
                f,
                "package refused: its parts expand to more than {} MiB (decompression-bomb guard)",
                MAX_TOTAL / (1024 * 1024)
            ),
        }
    }
}

/// Check `bytes` against the shipping limits.
pub(crate) fn check(bytes: &[u8]) -> Result<(), PackageLimit> {
    check_with(bytes, MAX_RATIO, RATIO_FLOOR, MAX_TOTAL)
}

fn check_with(bytes: &[u8], max_ratio: u64, ratio_floor: u64, max_total: u64) -> Result<(), PackageLimit> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(());
    };
    let mut total = 0u64;
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            return Ok(());
        };
        let per_entry = entry.compressed_size().saturating_mul(max_ratio).max(ratio_floor);
        let remaining = max_total - total;
        let mut meter = Meter { inflated: 0, limit: per_entry.min(remaining) };
        let copied = io::copy(&mut entry, &mut meter);
        if meter.inflated > meter.limit {
            return Err(if meter.limit == remaining && remaining < per_entry {
                PackageLimit::Total
            } else {
                PackageLimit::Ratio { part: entry.name().map(|n| n.into_owned()).unwrap_or_default() }
            });
        }
        if copied.is_err() {
            // A corrupt stream, a CRC mismatch, or an entry longer than its declared
            // size: the read fails. Leave it to the parser, which fails on the same entry.
            return Ok(());
        }
        total += meter.inflated;
    }
    Ok(())
}

/// A sink that counts bytes and fails once one more byte than `limit` arrives,
/// which ends the inflate early.
struct Meter {
    inflated: u64,
    limit: u64,
}

impl Write for Meter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inflated += buf.len() as u64;
        if self.inflated > self.limit {
            return Err(io::Error::other("inflate limit reached"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn package(parts: &[(&str, Vec<u8>)], method: zip::CompressionMethod) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(method);
        for (name, data) in parts {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn deflated(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
        package(parts, zip::CompressionMethod::Deflated)
    }

    #[test]
    fn ordinary_package_passes() {
        let xml = b"<w:document><w:body><w:p/></w:body></w:document>".to_vec();
        assert_eq!(check(&deflated(&[("word/document.xml", xml)])), Ok(()));
    }

    #[test]
    fn highly_compressible_part_below_the_floor_passes() {
        let bytes = deflated(&[("word/document.xml", vec![b'A'; 3 * 1024 * 1024])]);
        assert_eq!(check(&bytes), Ok(()));
    }

    #[test]
    fn highly_compressible_part_above_the_floor_is_refused() {
        let bytes = deflated(&[("word/document.xml", vec![b'A'; 5 * 1024 * 1024])]);
        assert_eq!(check(&bytes), Err(PackageLimit::Ratio { part: "word/document.xml".into() }));
    }

    #[test]
    fn total_ceiling_counts_every_part() {
        // Stored parts have ratio 1, so only the total can refuse them.
        let parts = [("a.xml", vec![b'x'; 600]), ("b.xml", vec![b'y'; 600])];
        let bytes = package(&parts, zip::CompressionMethod::Stored);
        assert_eq!(check_with(&bytes, MAX_RATIO, RATIO_FLOOR, 2000), Ok(()));
        assert_eq!(check_with(&bytes, MAX_RATIO, RATIO_FLOOR, 1000), Err(PackageLimit::Total));
    }

    #[test]
    fn forged_declared_size_does_not_hide_a_bomb() {
        let data = vec![b'A'; 5 * 1024 * 1024];
        let mut bytes = deflated(&[("word/document.xml", data)]);
        // Forge the uncompressed size to 16 bytes: local header (+22), central directory (+24).
        let forge = |bytes: &mut Vec<u8>, sig: &[u8; 4], off: usize| {
            let at = bytes.windows(4).position(|w| w == sig).unwrap() + off;
            bytes[at..at + 4].copy_from_slice(&16u32.to_le_bytes());
        };
        forge(&mut bytes, b"PK\x03\x04", 22);
        forge(&mut bytes, b"PK\x01\x02", 24);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        assert_eq!(archive.decompressed_size(), Some(16), "the forgery took");
        assert!(io::copy(&mut archive.by_index(0).unwrap(), &mut io::sink()).is_err(), "zip stops at the declared size");
        assert_eq!(check(&bytes), Ok(()), "the read error is left to the parser");
    }

    #[test]
    fn not_a_zip_is_left_to_the_parser() {
        assert_eq!(check(b"not a zip"), Ok(()));
    }
}
