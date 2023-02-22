///! An encapsulation of `BufReader`'s buffer management logic.
///
/// This module factors out the basic functionality of `BufReader` in order to protect two core
/// invariants:
/// * `filled` bytes of `buf` are always initialized
/// * `pos` is always <= `filled`
/// Since this module encapsulates the buffer management logic, we can ensure that the range
/// `pos..filled` is always a valid index into the initialized region of the buffer. This means
/// that user code which wants to do reads from a `BufReader` via `buffer` + `consume` can do so
/// without encountering any runtime bounds checks.
use crate::cmp;
use crate::io::{self, BorrowedBuf, Read};
use crate::mem::MaybeUninit;

pub(crate) struct Buffer {
    // The buffer.
    buf: Box<[MaybeUninit<u8>]>,
    // The current seek offset into `buf`, must always be <= `filled`.
    pos: usize,
    // Each call to `fill_buf` sets `filled` to indicate how many bytes at the start of `buf` are
    // initialized with bytes from a read.
    filled: usize,
    // This is the max number of bytes returned across all `fill_buf` calls. We track this so that we
    // can accurately tell `read_buf` how many bytes of buf are initialized, to bypass as much of its
    // defensive initialization as possible. Note that while this often the same as `filled`, it
    // doesn't need to be. Calls to `fill_buf` are not required to actually fill the buffer, and
    // omitting this is a huge perf regression for `Read` impls that do not.
    initialized: usize,
}

impl Buffer {
    #[inline]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let buf = Box::new_uninit_slice(capacity);
        Self { buf, pos: 0, filled: 0, initialized: 0 }
    }

    #[inline]
    pub(crate) fn buffer(&self) -> &[u8] {
        // SAFETY: self.pos and self.cap are valid, and self.cap => self.pos, and
        // that region is initialized because those are all invariants of this type.
        unsafe { MaybeUninit::slice_assume_init_ref(self.buf.get_unchecked(self.pos..self.filled)) }
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub(crate) fn filled(&self) -> usize {
        self.filled
    }

    #[inline]
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    // This is only used by a test which asserts that the initialization-tracking is correct.
    #[cfg(test)]
    pub(crate) fn initialized(&self) -> usize {
        self.initialized
    }

    #[inline]
    pub(crate) fn discard_buffer(&mut self) {
        self.pos = 0;
        self.filled = 0;
    }

    #[inline]
    pub(crate) fn consume(&mut self, amt: usize) {
        self.pos = cmp::min(self.pos + amt, self.filled);
    }

    /// If there are `amt` bytes available in the buffer, pass a slice containing those bytes to
    /// `visitor` and return true. If there are not enough bytes available, return false.
    #[inline]
    pub(crate) fn consume_with<V>(&mut self, amt: usize, mut visitor: V) -> bool
    where
        V: FnMut(&[u8]),
    {
        if let Some(claimed) = self.buffer().get(..amt) {
            visitor(claimed);
            // If the indexing into self.buffer() succeeds, amt must be a valid increment.
            self.pos += amt;
            true
        } else {
            false
        }
    }

    #[inline]
    pub(crate) fn unconsume(&mut self, amt: usize) {
        self.pos = self.pos.saturating_sub(amt);
    }

    #[inline]
    pub(crate) fn fill_buf(&mut self, mut reader: impl Read) -> io::Result<&[u8]> {
        // If we've reached the end of our internal buffer then we need to fetch
        // some more data from the reader.
        // Branch using `>=` instead of the more correct `==`
        // to tell the compiler that the pos..cap slice is always valid.
        if self.pos >= self.filled {
            debug_assert!(self.pos == self.filled);

            let mut buf = BorrowedBuf::from(&mut *self.buf);
            // SAFETY: `self.filled` bytes will always have been initialized.
            unsafe {
                buf.set_init(self.initialized);
            }

            reader.read_buf(buf.unfilled())?;

            self.pos = 0;
            self.filled = buf.len();
            self.initialized = buf.init_len();
        }
        Ok(self.buffer())
    }
}
