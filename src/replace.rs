//! Replace-All that streams the whole document through a regex into a brand new
//! file. Works even when the document is larger than RAM: we read line by line
//! and hold only one line at a time. (Matches that span a newline are out of
//! scope for streaming replace — that is a deliberate MVP limitation.)

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use regex::bytes::{Regex, RegexBuilder};

use crate::document::Snapshot;
use crate::search::SearchOpts;

#[derive(Debug)]
pub enum ReplaceMsg {
    Progress { bytes_done: u64, total: u64 },
    Done { replacements: u64, ms: f64, out: PathBuf },
    Error(String),
}

pub struct ReplaceHandle {
    pub cancel: Arc<AtomicBool>,
}

fn build_regex(opts: &SearchOpts) -> Result<Regex, String> {
    let mut pat = if opts.regex {
        opts.pattern.clone()
    } else {
        regex::escape(&opts.pattern)
    };
    if opts.whole_word && !opts.regex {
        pat = format!(r"\b(?:{})\b", pat);
    }
    RegexBuilder::new(&pat)
        .case_insensitive(!opts.case_sensitive)
        .build()
        .map_err(|e| e.to_string())
}

pub fn start(
    snap: Snapshot,
    opts: SearchOpts,
    replacement: Vec<u8>,
    out: PathBuf,
    tx: Sender<ReplaceMsg>,
    ctx: egui::Context,
) -> ReplaceHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = cancel.clone();

    thread::Builder::new()
        .name("gred-replace".into())
        .spawn(move || {
            let started = Instant::now();
            let re = match build_regex(&opts) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(ReplaceMsg::Error(e));
                    ctx.request_repaint();
                    return;
                }
            };

            let total = snap.len() as u64;
            let file = match File::create(&out) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(ReplaceMsg::Error(e.to_string()));
                    ctx.request_repaint();
                    return;
                }
            };
            let mut w = BufWriter::with_capacity(1 << 20, file);
            let mut r = BufReader::with_capacity(1 << 20, snap.reader(0));

            let mut line: Vec<u8> = Vec::with_capacity(256);
            let mut done: u64 = 0;
            let mut replacements: u64 = 0;
            let mut last_paint = Instant::now();

            loop {
                if cancel2.load(Ordering::SeqCst) {
                    let _ = tx.send(ReplaceMsg::Error("cancelled".into()));
                    ctx.request_repaint();
                    return;
                }
                line.clear();
                let n = match r.read_until(b'\n', &mut line) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = tx.send(ReplaceMsg::Error(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                };
                if n == 0 {
                    break;
                }
                done += n as u64;

                let hits = re.find_iter(&line).count() as u64;
                if hits > 0 {
                    replacements += hits;
                    let replaced = re.replace_all(&line, replacement.as_slice());
                    if let Err(e) = w.write_all(&replaced) {
                        let _ = tx.send(ReplaceMsg::Error(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                } else if let Err(e) = w.write_all(&line) {
                    let _ = tx.send(ReplaceMsg::Error(e.to_string()));
                    ctx.request_repaint();
                    return;
                }

                if last_paint.elapsed().as_millis() >= 100 {
                    let _ = tx.send(ReplaceMsg::Progress {
                        bytes_done: done,
                        total,
                    });
                    ctx.request_repaint();
                    last_paint = Instant::now();
                }
            }

            if let Err(e) = w.flush() {
                let _ = tx.send(ReplaceMsg::Error(e.to_string()));
                ctx.request_repaint();
                return;
            }

            let _ = tx.send(ReplaceMsg::Done {
                replacements,
                ms: started.elapsed().as_secs_f64() * 1000.0,
                out,
            });
            ctx.request_repaint();
        })
        .expect("spawn replace worker");

    ReplaceHandle { cancel }
}
