//! WordprocessingML (Word) renderer.
//!
//! All Word-specific layout/render code relocates here from `lib.rs`,
//! VERBATIM, organized by ECMA-376 chapter so a spec audit maps one chapter
//! to one module:
//!   bidi.rs      §17.3.1.6 / §17.3.2.30 + UAX#9 — RTL base direction & reorder
//!   border.rs    stands alone deliberately: border-edge RESOLUTION is shared
//!                by paragraph (§17.3.1.24) and page (§17.6.10) borders, which
//!                live in different spec chapters — a shared-mechanism module
//!                with § labels inside beats duplicating the resolver per
//!                chapter (cell borders §17.4 stay table-domain)
//!   (more chapters land here as the relocation proceeds: runs §17.3.2,
//!    paragraph §17.3.1, tables §17.4, sections §17.6, numbering §17.9,
//!    hdr/ftr §17.10, fields §17.16, plus the WML loader plumbing)
//!
//! Purity is enforced by the oracle snapshot tests — their geometry hashes
//! must not drift on a pure move.

pub(crate) mod bidi;
pub(crate) mod border;
pub(crate) mod drawing;
pub(crate) mod float;
pub(crate) mod float_resolve;
pub(crate) mod hdr_ftr;
pub(crate) mod linebreak;
pub(crate) mod loader;
pub(crate) mod math;
pub(crate) mod numbering;
pub(crate) mod page;
pub(crate) mod paragraph;
pub(crate) mod render;
pub(crate) mod section;
pub(crate) mod select;
pub(crate) mod table;
pub(crate) mod watermark;
