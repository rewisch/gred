//! Sparse line index, built lazily in the background.
//!
//! We never build a full line table. Instead a worker thread scans the document
//! for `\n`, recording the byte offset of every `STRIDE`-th line ("anchor").
//! Locating any line then means: jump to the nearest earlier anchor and scan
//! forward at most `STRIDE` lines. The UI never blocks on this: until the scan
//! reaches a line, the scrollbar simply doesn't extend that far yet.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use memchr::memchr_iter;

use crate::document::{Document, Snapshot};

/// Lines between successive anchors. 4096 lines * ~forward-scan is trivial work.
const STRIDE: u64 = 4096;
const CHUNK: usize = 1 << 20; // 1 MiB scan granularity

/// Shared, cheap-to-read byte access over "the document".
pub trait ByteSource {
    fn len_bytes(&self) -> usize;
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize;
}

impl ByteSource for Document {
    fn len_bytes(&self) -> usize {
        self.len()
    }
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.read_into(offset, buf)
    }
}

impl ByteSource for Snapshot {
    fn len_bytes(&self) -> usize {
        self.len()
    }
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.read_into(offset, buf)
    }
}

struct Inner {
    /// `anchors[k]` = document byte offset where line `k * STRIDE` begins.
    /// `anchors[0]` is always 0.
    anchors: Mutex<Vec<u64>>,
    /// Newlines counted so far (provisional line count is this + 1).
    newlines: AtomicU64,
    /// Document bytes scanned so far.
    scanned: AtomicU64,
    doc_len: AtomicU64,
    complete: AtomicBool,
    /// Bumped on every edit; a worker with a stale generation exits.
    generation: AtomicU64,
}

pub struct LineIndex {
    inner: Arc<Inner>,
    repaint: Option<egui::Context>,
}

/// Where a document offset sits, line-wise.
#[derive(Clone, Copy, Debug)]
pub struct Loc {
    pub line: u64,
    pub line_start: usize,
}

impl LineIndex {
    pub fn new() -> Self {
        LineIndex {
            inner: Arc::new(Inner {
                anchors: Mutex::new(vec![0]),
                newlines: AtomicU64::new(0),
                scanned: AtomicU64::new(0),
                doc_len: AtomicU64::new(0),
                complete: AtomicBool::new(true),
                generation: AtomicU64::new(0),
            }),
            repaint: None,
        }
    }

    /// (Re)start indexing for a fresh document.
    pub fn start(&mut self, snap: Snapshot, ctx: egui::Context) {
        self.repaint = Some(ctx.clone());
        {
            let mut a = self.inner.anchors.lock().unwrap();
            a.clear();
            a.push(0);
        }
        self.inner.newlines.store(0, Ordering::SeqCst);
        self.inner.scanned.store(0, Ordering::SeqCst);
        self.inner.doc_len.store(snap.len() as u64, Ordering::SeqCst);
        self.inner
            .complete
            .store(snap.is_empty(), Ordering::SeqCst);
        let gen = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !snap.is_empty() {
            spawn_worker(self.inner.clone(), snap, gen, 0, 0, Some(ctx));
        }
    }

    /// Tell the index an edit landed at `at`; keep all anchors strictly before it
    /// (byte offsets there are unchanged) and rescan forward from that point.
    pub fn notify_edit(&mut self, at: usize, snap: Snapshot) {
        let (resume_off, resume_newlines) = {
            let mut a = self.inner.anchors.lock().unwrap();
            let keep = a.partition_point(|&off| off < at as u64).max(1);
            a.truncate(keep);
            let resume_off = *a.last().unwrap();
            let resume_newlines = (a.len() as u64 - 1) * STRIDE;
            (resume_off, resume_newlines)
        };
        self.inner
            .newlines
            .store(resume_newlines, Ordering::SeqCst);
        self.inner
            .scanned
            .store(resume_off, Ordering::SeqCst);
        self.inner.doc_len.store(snap.len() as u64, Ordering::SeqCst);
        self.inner.complete.store(false, Ordering::SeqCst);
        let gen = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let ctx = self.repaint.clone();
        spawn_worker(
            self.inner.clone(),
            snap,
            gen,
            resume_off,
            resume_newlines,
            ctx,
        );
    }

    /// Provisional total line count (grows while indexing).
    pub fn total_lines(&self) -> u64 {
        self.inner.newlines.load(Ordering::Relaxed) + 1
    }

    pub fn is_complete(&self) -> bool {
        self.inner.complete.load(Ordering::Relaxed)
    }

    pub fn scanned_fraction(&self) -> f32 {
        let total = self.inner.doc_len.load(Ordering::Relaxed).max(1);
        (self.inner.scanned.load(Ordering::Relaxed) as f64 / total as f64) as f32
    }

    fn anchor_for_line(&self, line: u64) -> (u64, u64) {
        let a = self.inner.anchors.lock().unwrap();
        let ai = (line / STRIDE) as usize;
        if ai < a.len() {
            ((ai as u64) * STRIDE, a[ai])
        } else if !a.is_empty() {
            (((a.len() - 1) as u64) * STRIDE, a[a.len() - 1])
        } else {
            (0, 0)
        }
    }

    fn anchor_for_offset(&self, off: u64) -> (u64, u64) {
        let a = self.inner.anchors.lock().unwrap();
        // greatest anchor whose offset <= off
        let k = a.partition_point(|&v| v <= off).saturating_sub(1);
        ((k as u64) * STRIDE, a[k.min(a.len() - 1)])
    }

    /// Byte offset where `line` starts (clamped to EOF).
    pub fn line_start<B: ByteSource>(&self, src: &B, line: u64) -> usize {
        let (base_line, base_off) = self.anchor_for_line(line);
        let mut need = line.saturating_sub(base_line);
        let mut off = base_off as usize;
        let mut buf = vec![0u8; 64 * 1024];
        while need > 0 {
            let n = src.read_at(off, &mut buf);
            if n == 0 {
                break;
            }
            let mut advanced = 0usize;
            let mut hit = false;
            for p in memchr_iter(b'\n', &buf[..n]) {
                need -= 1;
                advanced = p + 1;
                if need == 0 {
                    hit = true;
                    break;
                }
            }
            if hit {
                off += advanced;
                return off;
            }
            off += n;
        }
        off.min(src.len_bytes())
    }

    /// Which line a document offset is on, and where that line starts.
    pub fn locate<B: ByteSource>(&self, src: &B, target: usize) -> Loc {
        let target = target.min(src.len_bytes());
        let (base_line, base_off) = self.anchor_for_offset(target as u64);
        let mut line = base_line;
        let mut off = base_off as usize;
        let mut line_start = off;
        let mut buf = vec![0u8; 64 * 1024];
        while off < target {
            let want = (target - off).min(buf.len());
            let n = src.read_at(off, &mut buf[..want]);
            if n == 0 {
                break;
            }
            for p in memchr_iter(b'\n', &buf[..n]) {
                if off + p >= target {
                    return Loc { line, line_start };
                }
                line += 1;
                line_start = off + p + 1;
            }
            off += n;
        }
        Loc { line, line_start }
    }

    /// Offset of the end of the line containing `from` (position of its `\n`, or EOF).
    pub fn line_end<B: ByteSource>(&self, src: &B, from: usize) -> usize {
        let mut off = from;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = src.read_at(off, &mut buf);
            if n == 0 {
                return src.len_bytes();
            }
            if let Some(p) = memchr::memchr(b'\n', &buf[..n]) {
                return off + p;
            }
            off += n;
        }
    }
}

fn spawn_worker(
    inner: Arc<Inner>,
    snap: Snapshot,
    generation: u64,
    start_off: u64,
    start_newlines: u64,
    ctx: Option<egui::Context>,
) {
    thread::Builder::new()
        .name("gred-lineindex".into())
        .spawn(move || {
            let len = snap.len() as u64;
            let mut off = start_off;
            let mut newlines = start_newlines;
            let mut buf = vec![0u8; CHUNK];
            let mut last_paint = std::time::Instant::now();
            // Stream the bytes rather than mmap them: keeps our working set flat
            // no matter how large the file is.
            let mut reader = snap.stream_reader_from(start_off as usize);

            while off < len {
                if inner.generation.load(Ordering::SeqCst) != generation {
                    return; // superseded by a newer edit
                }
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                for p in memchr_iter(b'\n', &buf[..n]) {
                    newlines += 1;
                    if newlines % STRIDE == 0 {
                        let anchor = off + p as u64 + 1;
                        let mut a = inner.anchors.lock().unwrap();
                        // index = newlines / STRIDE
                        if (a.len() as u64) == newlines / STRIDE {
                            a.push(anchor);
                        }
                    }
                }
                off += n as u64;
                inner.newlines.store(newlines, Ordering::Relaxed);
                inner.scanned.store(off, Ordering::Relaxed);

                if last_paint.elapsed() >= Duration::from_millis(100) {
                    if let Some(c) = &ctx {
                        c.request_repaint();
                    }
                    last_paint = std::time::Instant::now();
                }
                // Throttle so user interaction always wins the CPU.
                thread::sleep(Duration::from_micros(300));
            }

            if inner.generation.load(Ordering::SeqCst) == generation {
                inner.scanned.store(len, Ordering::Relaxed);
                inner.complete.store(true, Ordering::Relaxed);
                if let Some(c) = &ctx {
                    c.request_repaint();
                }
            }
        })
        .expect("spawn line index worker");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn doc(text: &[u8]) -> Document {
        let mut d = Document::new_empty();
        d.edit(0, 0, text);
        d
    }

    fn built(d: &Document) -> LineIndex {
        let mut li = LineIndex::new();
        li.start(d.snapshot(), egui::Context::default());
        for _ in 0..500 {
            if li.is_complete() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(li.is_complete(), "index did not finish");
        li
    }

    #[test]
    fn counts_and_offsets() {
        let d = doc(b"aaa\nbb\nc\ndddd\n");
        let li = built(&d);
        assert_eq!(li.total_lines(), 5); // trailing newline -> empty 5th line
        assert_eq!(li.line_start(&d, 0), 0);
        assert_eq!(li.line_start(&d, 1), 4);
        assert_eq!(li.line_start(&d, 2), 7);
        assert_eq!(li.line_start(&d, 3), 9);
        assert_eq!(li.line_start(&d, 4), 14);
    }

    #[test]
    fn locate_and_line_end() {
        let d = doc(b"hello\nworld\n!\n");
        let li = built(&d);
        let l = li.locate(&d, 8); // inside "world"
        assert_eq!(l.line, 1);
        assert_eq!(l.line_start, 6);
        assert_eq!(li.line_end(&d, l.line_start), 11); // position of the '\n'
        assert_eq!(li.locate(&d, 0).line, 0);
        assert_eq!(li.locate(&d, 12).line, 2);
    }

    #[test]
    fn large_multi_anchor() {
        // Enough lines to cross several STRIDE anchors.
        let n = (STRIDE as usize) * 3 + 17;
        let mut buf = Vec::new();
        for i in 0..n {
            buf.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        let d = doc(&buf);
        let li = built(&d);
        assert_eq!(li.total_lines(), n as u64 + 1);
        // spot-check an offset deep in the file
        let target_line = STRIDE * 2 + 5;
        let start = li.line_start(&d, target_line);
        let mut probe = vec![0u8; 16];
        let got = d.read_into(start, &mut probe);
        let s = std::str::from_utf8(&probe[..got]).unwrap();
        assert!(
            s.starts_with(&format!("line {}", target_line)),
            "got {s:?}"
        );
        assert_eq!(li.locate(&d, start).line, target_line);
    }
}
