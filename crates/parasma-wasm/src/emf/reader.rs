//! Bounded little-endian cursor over `&[u8]`. Every read is length-checked and
//! returns `None` past the end, so a truncated or malformed EMF aborts parsing
//! (`parse -> None`) and the caller falls back to the grey placeholder rather
//! than panicking.

pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Cursor<'a> {
        Cursor { buf, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub(crate) fn seek(&mut self, pos: usize) -> Option<()> {
        if pos <= self.buf.len() {
            self.pos = pos;
            Some(())
        } else {
            None
        }
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        if end > self.buf.len() {
            return None;
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Some(s)
    }
}

/// A standalone bounded slice reader over a record body (no cursor state).
pub(crate) struct Body<'a> {
    pub bytes: &'a [u8],
}

impl<'a> Body<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Body<'a> {
        Body { bytes }
    }

    pub(crate) fn u16(&self, off: usize) -> Option<u16> {
        let s = self.bytes.get(off..off + 2)?;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }

    pub(crate) fn u32(&self, off: usize) -> Option<u32> {
        let s = self.bytes.get(off..off + 4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub(crate) fn i32(&self, off: usize) -> Option<i32> {
        Some(self.u32(off)? as i32)
    }

    pub(crate) fn f32(&self, off: usize) -> Option<f32> {
        Some(f32::from_bits(self.u32(off)?))
    }

    pub(crate) fn slice(&self, off: usize, len: usize) -> Option<&'a [u8]> {
        self.bytes.get(off..off.checked_add(len)?)
    }
}
