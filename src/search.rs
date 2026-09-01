//! Streaming search built on the ripgrep libraries (`grep-regex` /
//! `grep-searcher`). Matches are pushed to the UI over a channel as they are
//! found, so the user can jump to the first hit without waiting for the full
//! scan. When the document is the untouched original file we search the mmap
//! slice directly (zero copy); otherwise we search a streaming reader over the
//! piece table.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};

use crate::document::Snapshot;

#[derive(Clone, Debug)]
pub struct SearchOpts {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug)]
pub enum SearchMsg {
    Hit(Hit),
    Done { total: usize, truncated: bool, ms: f64 },
    Error(String),
}

/// Hard cap so a pathological pattern can't exhaust memory.
pub const MAX_HITS: usize = 500_000;

pub struct SearchHandle {
    pub cancel: Arc<AtomicBool>,
}

impl SearchHandle {
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

pub fn build_matcher(opts: &SearchOpts) -> Result<RegexMatcher, String> {
    let pat = if opts.regex {
        opts.pattern.clone()
    } else {
        regex::escape(&opts.pattern)
    };
    RegexMatcherBuilder::new()
        .case_insensitive(!opts.case_sensitive)
        // Whole-word only applies to plain (non-regex) search, per the plan.
        .word(opts.whole_word && !opts.regex)
        .build(&pat)
        .map_err(|e| e.to_string())
}

pub fn start(
    snap: Snapshot,
    opts: SearchOpts,
    tx: Sender<SearchMsg>,
    ctx: egui::Context,
) -> SearchHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = cancel.clone();
    thread::Builder::new()
        .name("gred-search".into())
        .spawn(move || {
            let started = Instant::now();
            let matcher = match build_matcher(&opts) {
                Ok(m) => m,
                Err(e) => {
                    let _ = tx.send(SearchMsg::Error(e));
                    ctx.request_repaint();
                    return;
                }
            };
            let mut searcher: Searcher = SearcherBuilder::new()
                .line_number(false)
                .multi_line(false)
                .build();

            let mut sink = HitSink {
                matcher: &matcher,
                tx: &tx,
                ctx: &ctx,
                cancel: &cancel2,
                count: 0,
                truncated: false,
                last_paint: Instant::now(),
            };

            // Zero-copy over the mmap for modest files; stream larger ones so the
            // working set never tracks the file size (what ripgrep does too).
            const MMAP_SEARCH_LIMIT: usize = 256 << 20;
            let res = match snap.as_contiguous() {
                Some(bytes) if bytes.len() <= MMAP_SEARCH_LIMIT => {
                    searcher.search_slice(&matcher, bytes, &mut sink)
                }
                _ => searcher.search_reader(&matcher, snap.stream_reader(), &mut sink),
            };

            let total = sink.count;
            let truncated = sink.truncated;
            match res {
                Ok(()) => {
                    let _ = tx.send(SearchMsg::Done {
                        total,
                        truncated,
                        ms: started.elapsed().as_secs_f64() * 1000.0,
                    });
                }
                Err(e) => {
                    // A deliberate cancel surfaces as an error from the sink; treat
                    // "cancelled" as a normal stop.
                    if cancel2.load(Ordering::SeqCst) {
                        let _ = tx.send(SearchMsg::Done {
                            total,
                            truncated,
                            ms: started.elapsed().as_secs_f64() * 1000.0,
                        });
                    } else {
                        let _ = tx.send(SearchMsg::Error(e.to_string()));
                    }
                }
            }
            ctx.request_repaint();
        })
        .expect("spawn search worker");

    SearchHandle { cancel }
}

struct HitSink<'a> {
    matcher: &'a RegexMatcher,
    tx: &'a Sender<SearchMsg>,
    ctx: &'a egui::Context,
    cancel: &'a AtomicBool,
    count: usize,
    truncated: bool,
    last_paint: Instant,
}

impl<'a> Sink for HitSink<'a> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, m: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        if self.cancel.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let line = m.bytes();
        let line_abs = m.absolute_byte_offset() as usize;
        // grep hands us the whole line; pin the exact match span(s) within it.
        let mut at = 0usize;
        while at <= line.len() {
            match self
                .matcher
                .find_at(line, at)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            {
                Some(mm) => {
                    let start = mm.start();
                    let end = mm.end();
                    let _ = self.tx.send(SearchMsg::Hit(Hit {
                        offset: line_abs + start,
                        len: end.saturating_sub(start),
                    }));
                    self.count += 1;
                    if self.count >= MAX_HITS {
                        self.truncated = true;
                        return Ok(false);
                    }
                    at = if end > start { end } else { end + 1 };
                }
                None => break,
            }
        }

        if self.last_paint.elapsed().as_millis() >= 80 {
            self.ctx.request_repaint();
            self.last_paint = Instant::now();
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use std::sync::mpsc::channel;

    fn run_all(text: &[u8], opts: SearchOpts) -> Vec<Hit> {
        let mut d = Document::new_empty();
        d.edit(0, 0, text);
        let (tx, rx) = channel();
        let _h = start(d.snapshot(), opts, tx, egui::Context::default());
        let mut hits = Vec::new();
        loop {
            match rx.recv().unwrap() {
                SearchMsg::Hit(h) => hits.push(h),
                SearchMsg::Done { .. } => break,
                SearchMsg::Error(e) => panic!("search error: {e}"),
            }
        }
        hits
    }

    fn opts(pat: &str) -> SearchOpts {
        SearchOpts {
            pattern: pat.into(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
        }
    }

    #[test]
    fn plain_hits_have_exact_offsets() {
        let text = b"foo bar foo\nbar foo bar\n";
        let hits = run_all(text, opts("foo"));
        let offs: Vec<usize> = hits.iter().map(|h| h.offset).collect();
        assert_eq!(offs, vec![0, 8, 16]);
        assert!(hits.iter().all(|h| h.len == 3));
        for h in &hits {
            assert_eq!(&text[h.offset..h.offset + h.len], b"foo");
        }
    }

    #[test]
    fn case_sensitivity() {
        let text = b"Foo foo FOO";
        assert_eq!(run_all(text, opts("foo")).len(), 3);
        let cs = SearchOpts {
            case_sensitive: true,
            ..opts("foo")
        };
        assert_eq!(run_all(text, cs).len(), 1);
    }

    #[test]
    fn whole_word_and_regex() {
        let text = b"cat category scatter cat";
        let ww = SearchOpts {
            whole_word: true,
            ..opts("cat")
        };
        assert_eq!(run_all(text, ww).len(), 2);

        let re = SearchOpts {
            regex: true,
            ..opts(r"c\w+y")
        };
        let hits = run_all(text, re);
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].offset..hits[0].offset + hits[0].len], b"category");
    }

    #[test]
    fn multiple_hits_per_line() {
        let text = b"aaaa\n";
        let hits = run_all(text, opts("aa"));
        // non-overlapping
        assert_eq!(hits.iter().map(|h| h.offset).collect::<Vec<_>>(), vec![0, 2]);
    }
}
