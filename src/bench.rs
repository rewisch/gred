//! Headless benchmark / self-test harness.
//!
//!   gred --bench <file> [--search <pattern>]
//!   gred --gen <file> <size_gb>       # generate a synthetic test file
//!
//! Exercises the engine without a window and prints the numbers the plan asks
//! for: open-to-first-paint, line-index build, search throughput, scroll latency.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::channel;
use std::time::Instant;

use crate::document::Document;
use crate::lineindex::LineIndex;
use crate::search::{self, SearchMsg, SearchOpts};

pub fn main(args: &[String]) -> ! {
    let mut it = args.iter();
    let mode = it.next().map(|s| s.as_str()).unwrap_or("");

    match mode {
        "--gen" => {
            let path = it.next().expect("usage: --gen <file> <size_gb>");
            let gb: f64 = it
                .next()
                .and_then(|s| s.parse().ok())
                .expect("usage: --gen <file> <size_gb>");
            generate(Path::new(path), gb);
            std::process::exit(0);
        }
        "--bench" => {
            let path = it.next().expect("usage: --bench <file> [--search <pat>]");
            let mut pat = "error".to_string();
            while let Some(a) = it.next() {
                if a == "--search" {
                    if let Some(p) = it.next() {
                        pat = p.clone();
                    }
                }
            }
            bench(Path::new(path), &pat);
            std::process::exit(0);
        }
        _ => {
            eprintln!("bench: unknown mode {mode:?}");
            std::process::exit(2);
        }
    }
}

fn generate(path: &Path, gb: f64) {
    let target = (gb * (1u64 << 30) as f64) as u64;
    let f = std::fs::File::create(path).expect("create");
    let mut w = BufWriter::with_capacity(1 << 22, f);
    let mut written: u64 = 0;
    let mut n: u64 = 0;
    let t0 = Instant::now();
    // Varied line lengths, a sprinkling of a searchable token, some unicode.
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
    ];
    let mut buf = String::with_capacity(256);
    while written < target {
        buf.clear();
        let w1 = words[(n as usize) % words.len()];
        let w2 = words[(n as usize * 7 + 3) % words.len()];
        let _ = std::fmt::Write::write_fmt(
            &mut buf,
            format_args!("{n:09} {w1} {w2} line with some payload text"),
        );
        if n % 500 == 0 {
            buf.push_str(" ERROR needle-token café");
        }
        if n % 37 == 0 {
            buf.push_str(" tail extra columns to vary the width a bit more here");
        }
        buf.push('\n');
        w.write_all(buf.as_bytes()).expect("write");
        written += buf.len() as u64;
        n += 1;
    }
    w.flush().expect("flush");
    eprintln!(
        "generated {} ({:.2} GB, {} lines) in {:.1}s",
        path.display(),
        written as f64 / (1u64 << 30) as f64,
        n,
        t0.elapsed().as_secs_f64()
    );
}

fn bench(path: &Path, pattern: &str) {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    println!("file: {}  ({})", path.display(), human(size));
    println!("{}", "-".repeat(60));

    // ---- open -> first paint -------------------------------------------------
    let t0 = Instant::now();
    let doc = Document::open(path).expect("open");
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // "First paint" = read + decode the first screenful (say 60 lines).
    let tp = Instant::now();
    let first_screen = read_lines(&doc, 0, 60);
    let paint_ms = tp.elapsed().as_secs_f64() * 1000.0;
    println!("open():                 {open_ms:8.2} ms");
    println!("first 60 lines decoded: {paint_ms:8.2} ms   (open->first-paint = {:.2} ms)", open_ms + paint_ms);
    println!("  first line: {:?}", first_screen.first().map(|s| truncate(s, 72)));

    // ---- background line index --------------------------------------------
    let ctx = egui::Context::default();
    let mut lidx = LineIndex::new();
    let ti = Instant::now();
    lidx.start(doc.snapshot(), ctx.clone());
    // The worker is throttled on purpose; poll until complete.
    let mut ticks = 0;
    while !lidx.is_complete() {
        std::thread::sleep(std::time::Duration::from_millis(20));
        ticks += 1;
        if ti.elapsed().as_secs() > 600 {
            println!("line index: TIMED OUT after 600s");
            break;
        }
    }
    let idx_ms = ti.elapsed().as_secs_f64() * 1000.0;
    let lines = lidx.total_lines();
    println!(
        "line index build:       {idx_ms:8.2} ms   ({} lines, {:.1} MB/s, {} polls)",
        lines,
        (size as f64 / (1u64 << 20) as f64) / (idx_ms / 1000.0).max(1e-9),
        ticks
    );

    // ---- scroll latency: jump to random lines ----------------------------
    let mut rng = Rng(0x9E3779B97F4A7C15 ^ size);
    let mut worst = 0f64;
    let mut sum = 0f64;
    let samples = 200;
    for _ in 0..samples {
        let line = rng.next() % lines.max(1);
        let ts = Instant::now();
        let off = lidx.line_start(&doc, line);
        let _ = read_lines_from(&doc, off, 50);
        let us = ts.elapsed().as_secs_f64() * 1e6;
        worst = worst.max(us);
        sum += us;
    }
    println!(
        "scroll to random line:  {:8.2} us avg, {:8.2} us worst   ({} samples, 50 lines each)",
        sum / samples as f64,
        worst,
        samples
    );

    // ---- search throughput ---------------------------------------------
    let (tx, rx) = channel();
    let opts = SearchOpts {
        pattern: pattern.to_string(),
        case_sensitive: false,
        whole_word: false,
        regex: false,
    };
    let ts = Instant::now();
    let _h = search::start(doc.snapshot(), opts, tx, ctx.clone());
    let mut hits = 0u64;
    let mut done_ms = 0.0;
    loop {
        match rx.recv() {
            Ok(SearchMsg::Hit(_)) => hits += 1,
            Ok(SearchMsg::Done { total, ms, .. }) => {
                hits = total as u64;
                done_ms = ms;
                break;
            }
            Ok(SearchMsg::Error(e)) => {
                println!("search error: {e}");
                break;
            }
            Err(_) => break,
        }
    }
    let wall = ts.elapsed().as_secs_f64() * 1000.0;
    println!(
        "search {:?}:        {done_ms:8.2} ms  ({hits} hits, {:.1} MB/s, wall {wall:.0} ms)",
        pattern,
        (size as f64 / (1u64 << 20) as f64) / (done_ms / 1000.0).max(1e-9),
    );

    // ---- edit + save round-trip (small) --------------------------------
    let mut d2 = Document::open(path).expect("reopen");
    let te = Instant::now();
    for i in 0..1000 {
        let at = (i * 997) % d2.len().max(1);
        d2.edit(at, at, b"X");
    }
    println!(
        "1000 single-char edits: {:8.2} ms   (piece table)",
        te.elapsed().as_secs_f64() * 1000.0
    );

    println!("{}", "-".repeat(60));
    println!("logical memory: ~{} for the line-index anchors (STRIDE-sparse)", human(anchor_bytes(lines)));
    println!("(peak process RSS is reported by the PowerShell wrapper, if used)");
}

fn anchor_bytes(lines: u64) -> u64 {
    (lines / 4096 + 1) * 8
}

fn read_lines(doc: &Document, start: usize, n: usize) -> Vec<String> {
    read_lines_from(doc, start, n)
}

fn read_lines_from(doc: &Document, start: usize, n: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![0u8; 64 * 1024];
    let mut line = Vec::new();
    let mut off = start;
    while out.len() < n {
        let got = doc.read_into(off, &mut buf);
        if got == 0 {
            if !line.is_empty() {
                out.push(String::from_utf8_lossy(&line).into_owned());
            }
            break;
        }
        let mut i = 0;
        while i < got {
            match memchr::memchr(b'\n', &buf[i..got]) {
                Some(p) => {
                    line.extend_from_slice(&buf[i..i + p]);
                    out.push(String::from_utf8_lossy(&line).into_owned());
                    line.clear();
                    i += p + 1;
                    if out.len() >= n {
                        break;
                    }
                }
                None => {
                    line.extend_from_slice(&buf[i..got]);
                    i = got;
                }
            }
        }
        off += got;
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn human(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.2} {}", U[i])
    }
}

/// Tiny xorshift, deterministic across runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}
