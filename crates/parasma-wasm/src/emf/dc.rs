//! The EMF playback device context — the full GDI graphics state, cloned onto a
//! stack for SAVEDC/RESTOREDC, plus the object table for CREATE*/SELECTOBJECT.

use super::records::{
    LogBrush, LogFont, LogPen, PointL, BLACK_BRUSH, BLACK_PEN, NULL_BRUSH, NULL_PEN, STOCK_FLAG,
    WHITE_BRUSH,
};
use super::transform::{Mat, MapState};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum BkMode {
    Transparent,
    Opaque,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum HAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VBaseline {
    Top,
    Base,
    Bottom,
}

#[derive(Clone, Debug)]
pub(crate) enum GdiObject {
    Pen(LogPen),
    Brush(LogBrush),
    Font(LogFont),
}

#[derive(Clone, Debug)]
pub(crate) struct Dc {
    pub cur_pt: PointL,
    pub map: MapState,
    /// World transform as a column-vector `Mat` (SETWORLDTRANSFORM replaces it;
    /// MODIFYWORLDTRANSFORM composes into it — see `super::render`).
    pub world: Mat,
    pub pen: LogPen,
    pub brush: LogBrush,
    pub font: LogFont,
    pub text_color: String,
    pub bk_color: String,
    pub bk_mode: BkMode,
    pub h_align: HAlign,
    pub v_baseline: VBaseline,
    pub update_cp: bool,
    /// How many INTERSECTCLIPRECT `save()`s were opened at THIS save level, so a
    /// RESTOREDC issues the matching Painter `restore()`s.
    pub clip_depth: u32,
}

impl Default for Dc {
    fn default() -> Self {
        Dc {
            cur_pt: PointL { x: 0, y: 0 },
            map: MapState::default(),
            world: Mat::IDENTITY,
            pen: LogPen { style: 0, width: 0, color: "#000000".into() },
            brush: LogBrush { style: 0, color: "#ffffff".into() },
            font: LogFont { height: -12, weight: 400, italic: false, face: String::new() },
            text_color: "#000000".into(),
            bk_color: "#ffffff".into(),
            // GDI default is OPAQUE, but the renderer only draws an opaque box
            // when ETO_OPAQUE is set OR bk_mode==Opaque with a real bounds rect.
            bk_mode: BkMode::Opaque,
            h_align: HAlign::Left,
            v_baseline: VBaseline::Top,
            update_cp: false,
            clip_depth: 0,
        }
    }
}

/// The DC stack + object table + open clip count that survive across records.
pub(crate) struct DcStack {
    pub dc: Dc,
    stack: Vec<Dc>,
    objects: Vec<Option<GdiObject>>,
    /// Total number of currently-open clip `save()`s (across all levels), so
    /// picture-end can balance them.
    pub open_clips: u32,
}

impl DcStack {
    pub(crate) fn new(map: MapState) -> DcStack {
        let dc = Dc { map, ..Dc::default() };
        DcStack { dc, stack: Vec::new(), objects: Vec::new(), open_clips: 0 }
    }

    pub(crate) fn save(&mut self) {
        self.stack.push(self.dc.clone());
        // A fresh save level starts with no clips of its own.
        self.dc.clip_depth = 0;
    }

    /// Restore to a save target. `n` follows GDI RESTOREDC semantics: a negative
    /// value is relative (`-1` = the immediately-saved DC), a positive value is an
    /// absolute saved-DC index. Returns how many clip `save()`s must be undone.
    pub(crate) fn restore(&mut self, n: i32) -> u32 {
        // Number of levels to pop.
        let levels = if n < 0 {
            (-n) as usize
        } else if n >= 1 {
            // Absolute: pop down to level n. saved-DC ordinals are 1-based counting
            // from the bottom; translate to a pop count.
            self.stack.len().saturating_sub((n as usize).saturating_sub(1))
        } else {
            0
        };
        let mut undone = 0u32;
        for _ in 0..levels {
            // The clips opened at the CURRENT top level are discarded on the way up.
            undone += self.dc.clip_depth;
            match self.stack.pop() {
                Some(prev) => self.dc = prev,
                None => break,
            }
        }
        self.open_clips = self.open_clips.saturating_sub(undone);
        undone
    }

    /// Pop everything still on the stack at picture end (no state needed, just the
    /// number of clip saves that leaked so the renderer can balance them).
    pub(crate) fn take_open_clips(&self) -> u32 {
        self.open_clips
    }

    pub(crate) fn note_clip_opened(&mut self) {
        self.dc.clip_depth = self.dc.clip_depth.saturating_add(1);
        self.open_clips = self.open_clips.saturating_add(1);
    }

    pub(crate) fn create(&mut self, ih: u32, obj: GdiObject) {
        // CREATE* records name their OWN destination handle (not sequential).
        if ih & STOCK_FLAG != 0 {
            return; // never overwrite the stock range
        }
        // The table grows to the largest index seen, and that index comes from
        // the file. The header's `nHandles` is a 16-bit count (MS-EMF §2.2.9),
        // so no valid object index reaches 2^16; a larger one is ignored like a
        // bad index in `select`, instead of resizing the table to 2^31 slots.
        let ih = ih as usize;
        if ih > u16::MAX as usize {
            return;
        }
        if ih >= self.objects.len() {
            self.objects.resize(ih + 1, None);
        }
        self.objects[ih] = Some(obj);
    }

    pub(crate) fn delete(&mut self, ih: u32) {
        let ih = ih as usize;
        if let Some(slot) = self.objects.get_mut(ih) {
            *slot = None;
        }
    }

    /// Select an object into the matching DC slot. Handles the 0x80000000
    /// stock-object bit; a bad table index is ignored (leaves the slot unchanged).
    pub(crate) fn select(&mut self, ih: u32) {
        if ih & STOCK_FLAG != 0 {
            self.select_stock(ih);
            return;
        }
        let obj = self.objects.get(ih as usize).and_then(|o| o.clone());
        match obj {
            Some(GdiObject::Pen(p)) => self.dc.pen = p,
            Some(GdiObject::Brush(b)) => self.dc.brush = b,
            Some(GdiObject::Font(f)) => self.dc.font = f,
            None => {}
        }
    }

    fn select_stock(&mut self, ih: u32) {
        match ih {
            WHITE_BRUSH => self.dc.brush = LogBrush { style: 0, color: "#ffffff".into() },
            BLACK_BRUSH => self.dc.brush = LogBrush { style: 0, color: "#000000".into() },
            NULL_BRUSH => self.dc.brush = LogBrush { style: 1 /* BS_NULL */, color: "#ffffff".into() },
            BLACK_PEN => self.dc.pen = LogPen { style: 0, width: 1, color: "#000000".into() },
            NULL_PEN => self.dc.pen = LogPen { style: 5 /* PS_NULL */, width: 0, color: "#000000".into() },
            _ => {
                // DEVICE_DEFAULT_FONT / DEFAULT_GUI_FONT / others — leave a sane
                // default font; ignore for pen/brush.
            }
        }
    }
}
