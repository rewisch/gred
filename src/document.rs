//! File access + in-memory edit model.
//!
//! Core principle from the plan: never load the whole file. The original file is
//! memory-mapped (O(1) to "open"), and edits are layered on top as a piece
//! table. Reads walk the piece list; the OS pages in only what the viewport
//! touches. Memory stays flat regardless of file size.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Src {
    Orig,
    Add,
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    src: Src,
    start: usize,
    len: usize,
}

/// An immutable, cheaply-cloneable view of the document at a point in time.
///
/// Handed to background threads (search, line indexing) so they never touch the
/// live `Document`. `mmap` and `pieces` are shared via `Arc`; `add` is a small
/// buffer holding only inserted text, so cloning it per snapshot is cheap.
#[derive(Clone)]
pub struct Snapshot {
    mmap: Option<Arc<Mmap>>,
    add: Arc<Vec<u8>>,
    pieces: Arc<Vec<Piece>>,
    len: usize,
    /// Path of the original file, if the document came from one. Lets background
    /// workers stream the file with plain `ReadFile` instead of faulting the
    /// whole mapping into the working set.
    path: Option<Arc<PathBuf>>,
}

impl Snapshot {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True when the document is still exactly the untouched original file.
    pub fn is_pristine_file(&self) -> bool {
        self.path.is_some()
            && self.pieces.len() == 1
            && self.pieces[0].src == Src::Orig
            && self.pieces[0].start == 0
            && self.pieces[0].len == self.len
    }

    /// A sequential reader over the document starting at `start`, avoiding mmap
    /// when it can: for a pristine file it streams the file itself (flat working
    /// set); once edited it falls back to walking the piece table.
    pub fn stream_reader_from(&self, start: usize) -> Box<dyn Read + Send> {
        if self.is_pristine_file() {
            if let Some(p) = &self.path {
                if let Ok(mut f) = File::open(p.as_ref()) {
                    use std::io::{Seek, SeekFrom};
                    if start == 0 || f.seek(SeekFrom::Start(start as u64)).is_ok() {
                        return Box::new(std::io::BufReader::with_capacity(1 << 20, f));
                    }
                }
            }
        }
        Box::new(self.reader(start))
    }

    pub fn stream_reader(&self) -> Box<dyn Read + Send> {
        self.stream_reader_from(0)
    }

    fn piece_bytes<'a>(&'a self, p: &Piece) -> &'a [u8] {
        match p.src {
            Src::Orig => &self.mmap.as_ref().expect("orig piece without mmap")[p.start..p.start + p.len],
            Src::Add => &self.add[p.start..p.start + p.len],
        }
    }

    /// Copy up to `buf.len()` bytes starting at document offset `offset`.
    /// Returns the number of bytes copied (0 at/after EOF).
    pub fn read_into(&self, offset: usize, buf: &mut [u8]) -> usize {
        if offset >= self.len || buf.is_empty() {
            return 0;
        }
        let end = (offset + buf.len()).min(self.len);
        let mut written = 0usize;
        let mut cur = offset;
        let mut pos = 0usize;
        for p in self.pieces.iter() {
            if written == end - offset {
                break;
            }
            let pstart = pos;
            let pend = pos + p.len;
            pos = pend;
            if pend <= cur {
                continue;
            }
            let skip = cur - pstart;
            let avail = p.len - skip;
            let need = (end - cur).min(avail);
            let bytes = self.piece_bytes(p);
            buf[written..written + need].copy_from_slice(&bytes[skip..skip + need]);
            written += need;
            cur += need;
        }
        written
    }

    /// If the document is exactly the untouched original file, return the whole
    /// mmap slice so search can run zero-copy at ripgrep speed.
    pub fn as_contiguous(&self) -> Option<&[u8]> {
        match self.pieces.len() {
            0 => Some(&[]),
            1 => {
                let p = self.pieces[0];
                if p.src == Src::Orig {
                    self.mmap.as_ref().map(|m| &m[p.start..p.start + p.len])
                } else {
                    Some(&self.add[p.start..p.start + p.len])
                }
            }
            _ => None,
        }
    }

    /// A sequential `Read` over the whole document from `start`.
    pub fn reader(&self, start: usize) -> DocReader {
        let mut idx = 0usize;
        let mut off_in_piece = 0usize;
        let mut acc = 0usize;
        for (i, p) in self.pieces.iter().enumerate() {
            if start < acc + p.len {
                idx = i;
                off_in_piece = start - acc;
                acc = usize::MAX; // mark found
                break;
            }
            acc += p.len;
        }
        if acc != usize::MAX {
            // start >= len: position past the end
            idx = self.pieces.len();
            off_in_piece = 0;
        }
        DocReader {
            snap: self.clone(),
            idx,
            off_in_piece,
        }
    }

    /// Collect a (small) byte range into a Vec. Used for undo bookkeeping.
    pub fn collect_range(&self, start: usize, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len.min(self.len.saturating_sub(start))];
        let n = self.read_into(start, &mut v);
        v.truncate(n);
        v
    }
}

pub struct DocReader {
    snap: Snapshot,
    idx: usize,
    off_in_piece: usize,
}

impl Read for DocReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.idx >= self.snap.pieces.len() {
                return Ok(0);
            }
            let p = self.snap.pieces[self.idx];
            let bytes = self.snap.piece_bytes(&p);
            if self.off_in_piece >= bytes.len() {
                self.idx += 1;
                self.off_in_piece = 0;
                continue;
            }
            let n = (bytes.len() - self.off_in_piece).min(buf.len());
            buf[..n].copy_from_slice(&bytes[self.off_in_piece..self.off_in_piece + n]);
            self.off_in_piece += n;
            return Ok(n);
        }
    }
}

/// Opaque undo/redo handle. See [`Document::checkpoint`].
#[derive(Clone)]
pub struct Checkpoint {
    pieces: Arc<Vec<Piece>>,
    len: usize,
}

/// Result of an `edit`, used to nudge the line index and search state.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct EditResult {
    pub at: usize,
    pub removed_len: usize,
    pub inserted_len: usize,
}

pub struct Document {
    mmap: Option<Arc<Mmap>>,
    /// Append-only until `save`. Historical piece lists stay valid against it.
    add: Vec<u8>,
    pieces: Arc<Vec<Piece>>,
    len: usize,
    pub path: Option<PathBuf>,
    pub dirty: bool,
}

impl Document {
    pub fn new_empty() -> Self {
        Document {
            mmap: None,
            add: Vec::new(),
            pieces: Arc::new(Vec::new()),
            len: 0,
            path: None,
            dirty: false,
        }
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len() as usize;
        let mmap = if size == 0 {
            None
        } else {
            // SAFETY: we accept the standard mmap caveat (external truncation is UB).
            Some(Arc::new(unsafe { Mmap::map(&file)? }))
        };
        let pieces = if size == 0 {
            Vec::new()
        } else {
            vec![Piece {
                src: Src::Orig,
                start: 0,
                len: size,
            }]
        };
        Ok(Document {
            mmap,
            add: Vec::new(),
            pieces: Arc::new(pieces),
            len: size,
            path: Some(path.to_path_buf()),
            dirty: false,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            mmap: self.mmap.clone(),
            add: Arc::new(self.add.clone()),
            pieces: self.pieces.clone(),
            len: self.len,
            path: self.path.clone().map(Arc::new),
        }
    }

    /// A cheap point-in-time handle to the document contents, for undo/redo.
    /// Only shares `Arc`s; costs nothing to keep around.
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            pieces: self.pieces.clone(),
            len: self.len,
        }
    }

    /// Restore contents from a checkpoint. The append-only `add` buffer still
    /// backs any Add pieces the checkpoint references.
    pub fn restore(&mut self, cp: &Checkpoint, dirty: bool) {
        self.pieces = cp.pieces.clone();
        self.len = cp.len;
        self.dirty = dirty;
    }

    pub fn read_into(&self, offset: usize, buf: &mut [u8]) -> usize {
        // Reuse the snapshot logic without allocating a fresh add clone.
        if offset >= self.len || buf.is_empty() {
            return 0;
        }
        let end = (offset + buf.len()).min(self.len);
        let mut written = 0usize;
        let mut cur = offset;
        let mut pos = 0usize;
        for p in self.pieces.iter() {
            if written == end - offset {
                break;
            }
            let pstart = pos;
            let pend = pos + p.len;
            pos = pend;
            if pend <= cur {
                continue;
            }
            let skip = cur - pstart;
            let avail = p.len - skip;
            let need = (end - cur).min(avail);
            let bytes: &[u8] = match p.src {
                Src::Orig => &self.mmap.as_ref().unwrap()[p.start + skip..p.start + skip + need],
                Src::Add => &self.add[p.start + skip..p.start + skip + need],
            };
            buf[written..written + need].copy_from_slice(bytes);
            written += need;
            cur += need;
        }
        written
    }

    /// Replace document byte range `[start, end)` with `repl`.
    pub fn edit(&mut self, start: usize, end: usize, repl: &[u8]) -> EditResult {
        let start = start.min(self.len);
        let end = end.min(self.len).max(start);

        let add_off = self.add.len();
        self.add.extend_from_slice(repl);

        let old: &Vec<Piece> = &self.pieces;
        let mut before: Vec<Piece> = Vec::with_capacity(old.len() + 1);
        let mut after: Vec<Piece> = Vec::new();
        let mut pos = 0usize;
        for p in old.iter() {
            let pstart = pos;
            let pend = pos + p.len;
            pos = pend;
            if pstart < start {
                let e = pend.min(start);
                before.push(Piece {
                    src: p.src,
                    start: p.start,
                    len: e - pstart,
                });
            }
            if pend > end {
                let s = pstart.max(end);
                let within = s - pstart;
                after.push(Piece {
                    src: p.src,
                    start: p.start + within,
                    len: pend - s,
                });
            }
        }

        let mut coalesced = false;
        if !repl.is_empty() {
            if let Some(last) = before.last_mut() {
                if last.src == Src::Add && last.start + last.len == add_off {
                    // Sequential typing: extend the trailing add piece in place.
                    last.len += repl.len();
                    coalesced = true;
                }
            }
        }

        let mut out: Vec<Piece> = before;
        if !repl.is_empty() && !coalesced {
            out.push(Piece {
                src: Src::Add,
                start: add_off,
                len: repl.len(),
            });
        }
        out.extend(after);

        self.pieces = Arc::new(out);
        self.len = self.len - (end - start) + repl.len();
        self.dirty = true;

        EditResult {
            at: start,
            removed_len: end - start,
            inserted_len: repl.len(),
        }
    }

    /// Stream every piece sequentially into `path` via a temp file, then replace.
    /// Works for documents larger than RAM. Afterwards the document is re-mapped
    /// onto the freshly written file so memory returns to a flat baseline.
    pub fn save(&mut self, path: &Path) -> io::Result<u64> {
        let tmp = {
            let mut t = path.as_os_str().to_owned();
            t.push(".gred.tmp");
            PathBuf::from(t)
        };

        let mut written: u64 = 0;
        {
            let f = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            for p in self.pieces.iter() {
                let bytes: &[u8] = match p.src {
                    Src::Orig => &self.mmap.as_ref().unwrap()[p.start..p.start + p.len],
                    Src::Add => &self.add[p.start..p.start + p.len],
                };
                w.write_all(bytes)?;
                written += bytes.len() as u64;
            }
            w.flush()?;
            let _ = w.get_ref().sync_all();
        }

        // Drop the old mapping before replacing the file it points at (Windows
        // won't rename over a mapped file).
        let had_mmap = self.mmap.is_some();
        let old_path = self.path.clone();
        self.mmap = None;
        let replaced = std::fs::rename(&tmp, path).or_else(|_| {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)
        });
        if let Err(e) = replaced {
            // Put the document back into a usable state before returning.
            let _ = std::fs::remove_file(&tmp);
            if had_mmap {
                if let Some(op) = &old_path {
                    if let Ok(of) = File::open(op) {
                        if let Ok(mm) = unsafe { Mmap::map(&of) } {
                            self.mmap = Some(Arc::new(mm));
                        }
                    }
                }
            }
            return Err(e);
        }

        let file = File::open(path)?;
        let size = file.metadata()?.len() as usize;
        self.mmap = if size == 0 {
            None
        } else {
            Some(Arc::new(unsafe { Mmap::map(&file)? }))
        };
        self.add.clear();
        self.pieces = Arc::new(if size == 0 {
            Vec::new()
        } else {
            vec![Piece {
                src: Src::Orig,
                start: 0,
                len: size,
            }]
        });
        self.len = size;
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_bytes(b: &[u8]) -> Document {
        // A document with no file: seed it via one edit into the empty doc.
        let mut d = Document::new_empty();
        d.edit(0, 0, b);
        d.dirty = false;
        d
    }

    fn whole(d: &Document) -> Vec<u8> {
        let mut v = vec![0u8; d.len()];
        let n = d.read_into(0, &mut v);
        v.truncate(n);
        v
    }

    #[test]
    fn read_into_partial_and_bounds() {
        let d = from_bytes(b"hello world");
        let mut buf = [0u8; 5];
        assert_eq!(d.read_into(0, &mut buf), 5);
        assert_eq!(&buf, b"hello");
        assert_eq!(d.read_into(6, &mut buf), 5);
        assert_eq!(&buf, b"world");
        assert_eq!(d.read_into(11, &mut buf), 0);
        assert_eq!(d.read_into(100, &mut buf), 0);
    }

    #[test]
    fn insert_middle_end_start() {
        let mut d = from_bytes(b"AC");
        d.edit(1, 1, b"B"); // insert
        assert_eq!(whole(&d), b"ABC");
        d.edit(3, 3, b"D"); // append
        assert_eq!(whole(&d), b"ABCD");
        d.edit(0, 0, b"@"); // prepend
        assert_eq!(whole(&d), b"@ABCD");
        assert_eq!(d.len(), 5);
    }

    #[test]
    fn delete_and_replace_spanning_pieces() {
        let mut d = from_bytes(b"the quick brown fox");
        d.edit(4, 9, b""); // remove "quick"
        assert_eq!(whole(&d), b"the  brown fox");
        d.edit(0, 3, b"THE"); // replace head
        assert_eq!(whole(&d), b"THE  brown fox");
        d.edit(5, 10, b"green"); // replace inside
        assert_eq!(whole(&d), b"THE  green fox");
    }

    #[test]
    fn sequential_typing_coalesces_pieces() {
        let mut d = from_bytes(b"");
        for (i, ch) in b"abcdefghij".iter().enumerate() {
            d.edit(i, i, std::slice::from_ref(ch));
        }
        assert_eq!(whole(&d), b"abcdefghij");
        // The append-buffer inserts should collapse to a single piece.
        assert_eq!(d.pieces.len(), 1);
    }

    #[test]
    fn checkpoint_restore_roundtrip() {
        let mut d = from_bytes(b"one two three");
        let cp = d.checkpoint();
        d.edit(0, 3, b"XXX");
        d.edit(0, 0, b"prefix ");
        assert_eq!(whole(&d), b"prefix XXX two three");
        d.restore(&cp, false);
        assert_eq!(whole(&d), b"one two three");
    }

    #[test]
    fn doc_reader_matches_read_into() {
        let mut d = from_bytes(b"line one\nline two\nline three\n");
        d.edit(9, 9, b">>> ");
        let snap = d.snapshot();
        let mut via_reader = Vec::new();
        snap.reader(0).read_to_end(&mut via_reader).unwrap();
        assert_eq!(via_reader, whole(&d));
        // reader from an offset
        let mut tail = Vec::new();
        snap.reader(13).read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &whole(&d)[13..]);
    }

    #[test]
    fn save_reopens_flat() {
        let dir = std::env::temp_dir().join(format!("gred_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("save.txt");
        std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();

        let mut d = Document::open(&path).unwrap();
        d.edit(6, 10, b"BETA"); // beta -> BETA
        d.edit(0, 0, b"# header\n");
        assert!(d.dirty);
        let written = d.save(&path).unwrap();
        assert_eq!(written as usize, d.len());
        assert!(!d.dirty);
        // After save it is a single pristine piece again.
        assert_eq!(d.pieces.len(), 1);
        assert!(d.snapshot().is_pristine_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"# header\nalpha\nBETA\ngamma\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
