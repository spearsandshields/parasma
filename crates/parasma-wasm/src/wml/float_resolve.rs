//! Anchor-position resolution for floating/anchored drawings (§20.4.2.13/.14).
//! Single source of truth for the reference-frame arithmetic that maps an
//! anchored object's H/V relative-from + offset/alignment to an absolute
//! top-left in page px. Extracted verbatim from the three triplicated inline
//! sites in `lib.rs` during the float-layout refactor. The per-`AnchorPos`
//! x/y math still lives in `resolve_anchor_xy`; this layer only computes the
//! h_base/h_extent/v_base/v_extent bases from the frame context, exactly as
//! the inline sites did.
//!
//! Purity is enforced by the oracle snapshot tests — geometry hashes must not
//! drift on this pure move.
//!
//! NOTE: the `_` arms (h_base/h_extent/v_extent) and the `Paragraph | Line`
//! v_base arm rely on `AnchorHRelFrom`/`AnchorVRelFrom` being the closed 4+4
//! enums in `float.rs`. If a 5th variant is added, re-audit each `_` arm.

use crate::wml::float::{
    resolve_anchor_xy, AnchorHRelFrom, AnchorPos, AnchorVRelFrom,
};

/// The caller-scope reference frame an anchored object resolves against: the
/// current column's left edge/width, the page box, the effective top margin,
/// the paragraph's y within the content box, and the content height. All
/// fields are page-space px, already computed by the caller (the resolver does
/// no margin/column lookups itself).
pub(crate) struct FrameCtx {
    pub col_x_offset: f64,
    pub col_width: f64,
    pub page_w: f64,
    pub page_height: f64,
    pub eff_margin_top: f64,
    pub y_in_page: f64,
    pub content_height: f64,
}

/// Resolve an anchored object's top-left to absolute page px from its H/V
/// relative-from + position children and the surrounding frame. Reproduces the
/// inline base/extent selection the three page-assembly sites performed before
/// delegating to [`resolve_anchor_xy`].
pub(crate) fn resolve_anchor_position(
    pos_h: &AnchorPos,
    pos_v: &AnchorPos,
    h_rel: AnchorHRelFrom,
    v_rel: AnchorVRelFrom,
    w: f64,
    h: f64,
    ctx: &FrameCtx,
) -> (f64, f64) {
    let h_base = match h_rel {
        AnchorHRelFrom::Page => 0.0,
        _ => ctx.col_x_offset,
    };
    let h_extent = match h_rel {
        AnchorHRelFrom::Page => ctx.page_w,
        _ => ctx.col_width,
    };
    let v_base = match v_rel {
        AnchorVRelFrom::Page => 0.0,
        AnchorVRelFrom::Margin => ctx.eff_margin_top,
        AnchorVRelFrom::Paragraph | AnchorVRelFrom::Line => ctx.eff_margin_top + ctx.y_in_page,
    };
    let v_extent = match v_rel {
        AnchorVRelFrom::Page => ctx.page_height,
        _ => ctx.content_height,
    };
    resolve_anchor_xy(pos_h, pos_v, w, h, h_base, h_extent, v_base, v_extent)
}
