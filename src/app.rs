//! The egui front-end: virtualized text grid, cursor/selection, editing,
//! find/replace bar, status bar. Only the visible rows are ever built or
//! painted, so scrolling cost is independent of file size.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Key, Modifiers, Pos2, Rect, Stroke, Vec2};

use crate::document::Document;
use crate::lineindex::LineIndex;
use crate::replace::{self, ReplaceHandle, ReplaceMsg};
use crate::search::{self, Hit, SearchHandle, SearchMsg, SearchOpts};

const TAB_W: usize = 4;
const MAX_LINE_CHARS: usize = 4000;
const UNDO_LIMIT: usize = 400;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------
struct Theme {
    bg: Color32,
    gutter_bg: Color32,
    gutter_fg: Color32,
    fg: Color32,
    caret: Color32,
    sel: Color32,
    hit: Color32,
    hit_current: Color32,
    scrollbar: Color32,
    scrollbar_bg: Color32,
}

impl Theme {
    fn dark() -> Self {
        Theme {
            bg: Color32::from_rgb(0x1e, 0x1e, 0x1e),
            gutter_bg: Color32::from_rgb(0x25, 0x25, 0x26),
            gutter_fg: Color32::from_rgb(0x85, 0x85, 0x85),
            fg: Color32::from_rgb(0xd4, 0xd4, 0xd4),
            caret: Color32::from_rgb(0xe8, 0xe8, 0xe8),
            sel: Color32::from_rgba_unmultiplied(0x26, 0x4f, 0x78, 0xff),
            hit: Color32::from_rgba_unmultiplied(0x5a, 0x50, 0x00, 0xff),
            hit_current: Color32::from_rgba_unmultiplied(0x9e, 0x6a, 0x00, 0xff),
            scrollbar: Color32::from_rgb(0x5a, 0x5a, 0x5a),
            scrollbar_bg: Color32::from_rgb(0x2a, 0x2a, 0x2a),
        }
    }
}

// ---------------------------------------------------------------------------
// Visible line model
// ---------------------------------------------------------------------------
struct VLine {
    /// doc offset of the first byte of the line
    start: usize,
    /// doc offset just past the last content byte (caret "End" position)
    content_end: usize,
    /// doc offset of the start of the next line (past the newline)
    next_line: usize,
    /// display text (tabs expanded, control chars shown)
    disp: String,
    /// map[i] = doc byte offset of display char i; last entry = content_end
    map: Vec<usize>,
}

impl VLine {
    fn cols(&self) -> usize {
        self.map.len().saturating_sub(1)
    }
    /// display column (char count) for a doc offset inside this line
    fn col_of(&self, doc_off: usize) -> usize {
        self.map.partition_point(|&b| b < doc_off)
    }
    /// doc offset nearest to display column `col`
    fn off_of_col(&self, col: usize) -> usize {
        let ci = col.min(self.map.len().saturating_sub(1));
        self.map[ci]
    }
}

// ---------------------------------------------------------------------------
// Async job bookkeeping
// ---------------------------------------------------------------------------
struct SearchJob {
    rx: Receiver<SearchMsg>,
    handle: SearchHandle,
    running: bool,
    truncated: bool,
    ms: f64,
}

struct ReplaceJob {
    rx: Receiver<ReplaceMsg>,
    handle: ReplaceHandle,
    bytes_done: u64,
    total: u64,
}

#[derive(Clone)]
struct UndoEntry {
    cp: crate::document::Checkpoint,
    caret: usize,
    anchor: usize,
    dirty: bool,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------
pub struct Gred {
    theme: Theme,
    doc: Document,
    lidx: LineIndex,

    // viewport
    top_line: f64,
    left_col: f32,
    font_size: f32,
    visible_rows: usize,

    // cursor (document byte offsets); caret == anchor => no selection
    caret: usize,
    anchor: usize,
    desired_col: Option<usize>,
    blink_phase: f32,

    // find / replace
    show_find: bool,
    replace_mode: bool,
    find_text: String,
    replace_text: String,
    opt_case: bool,
    opt_word: bool,
    opt_regex: bool,
    opt_wrap: bool,
    focus_find: bool,
    hits: Vec<Hit>,
    current_hit: Option<usize>,
    search_job: Option<SearchJob>,
    replace_job: Option<ReplaceJob>,

    // goto
    show_goto: bool,
    goto_text: String,
    focus_goto: bool,

    // undo / redo
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,

    // status
    status: String,
    open_pending: Option<Instant>,
    open_ms: Option<f64>,
    last_title: String,
}

impl Gred {
    pub fn new(cc: &eframe::CreationContext<'_>, open_path: Option<String>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Gred {
            theme: Theme::dark(),
            doc: Document::new_empty(),
            lidx: LineIndex::new(),
            top_line: 0.0,
            left_col: 0.0,
            font_size: 14.0,
            visible_rows: 40,
            caret: 0,
            anchor: 0,
            desired_col: None,
            blink_phase: 0.0,
            show_find: false,
            replace_mode: false,
            find_text: String::new(),
            replace_text: String::new(),
            opt_case: false,
            opt_word: false,
            opt_regex: false,
            opt_wrap: true,
            focus_find: false,
            hits: Vec::new(),
            current_hit: None,
            search_job: None,
            replace_job: None,
            show_goto: false,
            goto_text: String::new(),
            focus_goto: false,
            undo: Vec::new(),
            redo: Vec::new(),
            status: "Ready — Ctrl+O to open".into(),
            open_pending: None,
            open_ms: None,
            last_title: String::new(),
        };
        if let Some(p) = open_path {
            app.open_path(&cc.egui_ctx, PathBuf::from(p));
        }
        app
    }

    // ---- file ops ------------------------------------------------------------

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.open_path(ctx, path);
        }
    }

    fn open_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        let t0 = Instant::now();
        match Document::open(&path) {
            Ok(d) => {
                self.doc = d;
                self.caret = 0;
                self.anchor = 0;
                self.top_line = 0.0;
                self.left_col = 0.0;
                self.desired_col = None;
                self.undo.clear();
                self.redo.clear();
                self.hits.clear();
                self.current_hit = None;
                self.search_job = None;
                self.lidx = LineIndex::new();
                self.lidx.start(self.doc.snapshot(), ctx.clone());
                self.open_pending = Some(t0);
                self.open_ms = None;
                self.status = format!(
                    "Opened {} ({})",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    human_bytes(self.doc.len() as u64)
                );
            }
            Err(e) => {
                self.status = format!("Open failed: {e}");
            }
        }
    }

    fn save(&mut self, ctx: &egui::Context, save_as: bool) {
        let path = if save_as || self.doc.path.is_none() {
            match rfd::FileDialog::new().save_file() {
                Some(p) => p,
                None => return,
            }
        } else {
            self.doc.path.clone().unwrap()
        };
        let t0 = Instant::now();
        match self.doc.save(&path) {
            Ok(bytes) => {
                // Content is unchanged on disk; the line index stays valid.
                self.status = format!(
                    "Saved {} in {:.0} ms",
                    human_bytes(bytes),
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                let _ = ctx;
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    // ---- editing ----------------------------------------------------------

    fn sel_range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }
    fn has_sel(&self) -> bool {
        self.caret != self.anchor
    }

    fn push_undo(&mut self) {
        self.undo.push(UndoEntry {
            cp: self.doc.checkpoint(),
            caret: self.caret,
            anchor: self.anchor,
            dirty: self.doc.dirty,
        });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn replace_selection(&mut self, bytes: &[u8]) {
        let (s, e) = self.sel_range();
        self.push_undo();
        self.doc.edit(s, e, bytes);
        self.caret = s + bytes.len();
        self.anchor = self.caret;
        self.desired_col = None;
        self.lidx.notify_edit(s, self.doc.snapshot());
        self.invalidate_search();
        self.blink_phase = 0.0;
    }

    fn backspace(&mut self) {
        if self.has_sel() {
            self.replace_selection(b"");
        } else if self.caret > 0 {
            let p = char_boundary_before(&self.doc, self.caret);
            self.push_undo();
            self.doc.edit(p, self.caret, b"");
            self.caret = p;
            self.anchor = p;
            self.desired_col = None;
            self.lidx.notify_edit(p, self.doc.snapshot());
            self.invalidate_search();
        }
    }

    fn delete_fwd(&mut self) {
        if self.has_sel() {
            self.replace_selection(b"");
        } else if self.caret < self.doc.len() {
            let n = char_boundary_after(&self.doc, self.caret);
            self.push_undo();
            self.doc.edit(self.caret, n, b"");
            self.anchor = self.caret;
            self.desired_col = None;
            self.lidx.notify_edit(self.caret, self.doc.snapshot());
            self.invalidate_search();
        }
    }

    fn do_undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(UndoEntry {
                cp: self.doc.checkpoint(),
                caret: self.caret,
                anchor: self.anchor,
                dirty: self.doc.dirty,
            });
            self.doc.restore(&prev.cp, prev.dirty);
            self.caret = prev.caret.min(self.doc.len());
            self.anchor = prev.anchor.min(self.doc.len());
            self.desired_col = None;
            self.lidx.notify_edit(0, self.doc.snapshot());
            self.invalidate_search();
            self.status = "Undo".into();
        }
    }

    fn do_redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(UndoEntry {
                cp: self.doc.checkpoint(),
                caret: self.caret,
                anchor: self.anchor,
                dirty: self.doc.dirty,
            });
            self.doc.restore(&next.cp, next.dirty);
            self.caret = next.caret.min(self.doc.len());
            self.anchor = next.anchor.min(self.doc.len());
            self.desired_col = None;
            self.lidx.notify_edit(0, self.doc.snapshot());
            self.invalidate_search();
            self.status = "Redo".into();
        }
    }

    fn selection_string(&self) -> String {
        let (s, e) = self.sel_range();
        if s == e {
            return String::new();
        }
        let bytes = self.doc.snapshot().collect_range(s, e - s);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // ---- search ----------------------------------------------------------

    fn invalidate_search(&mut self) {
        if !self.hits.is_empty() || self.search_job.is_some() {
            if let Some(job) = &self.search_job {
                job.handle.stop();
            }
            self.search_job = None;
            self.hits.clear();
            self.current_hit = None;
        }
    }

    fn opts(&self) -> SearchOpts {
        SearchOpts {
            pattern: self.find_text.clone(),
            case_sensitive: self.opt_case,
            whole_word: self.opt_word,
            regex: self.opt_regex,
        }
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        if self.find_text.is_empty() {
            return;
        }
        if let Some(job) = &self.search_job {
            job.handle.stop();
        }
        self.hits.clear();
        self.current_hit = None;
        let (tx, rx) = channel();
        let handle = search::start(self.doc.snapshot(), self.opts(), tx, ctx.clone());
        self.search_job = Some(SearchJob {
            rx,
            handle,
            running: true,
            truncated: false,
            ms: 0.0,
        });
        self.status = "Searching…".into();
    }

    fn pump_async(&mut self, ctx: &egui::Context) {
        // open-to-first-paint measurement
        if let Some(t0) = self.open_pending.take() {
            self.open_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let mut jump_first = false;
        if let Some(job) = &mut self.search_job {
            let mut got = 0;
            loop {
                match job.rx.try_recv() {
                    Ok(SearchMsg::Hit(h)) => {
                        if self.hits.is_empty() {
                            jump_first = true;
                        }
                        self.hits.push(h);
                        got += 1;
                        if got > 20_000 {
                            break; // keep the frame snappy; rest next frame
                        }
                    }
                    Ok(SearchMsg::Done {
                        total,
                        truncated,
                        ms,
                    }) => {
                        job.running = false;
                        job.truncated = truncated;
                        job.ms = ms;
                        self.status = format!(
                            "{} match{} in {:.0} ms{}",
                            total,
                            if total == 1 { "" } else { "es" },
                            ms,
                            if truncated { " (capped)" } else { "" }
                        );
                        break;
                    }
                    Ok(SearchMsg::Error(e)) => {
                        job.running = false;
                        self.status = format!("Search error: {e}");
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        job.running = false;
                        break;
                    }
                }
            }
            if job.running {
                ctx.request_repaint_after(Duration::from_millis(33));
            }
        }
        if jump_first {
            self.goto_hit(0, ctx);
        }

        // replace-all streaming
        let mut clear_replace = false;
        if let Some(job) = &mut self.replace_job {
            loop {
                match job.rx.try_recv() {
                    Ok(ReplaceMsg::Progress { bytes_done, total }) => {
                        job.bytes_done = bytes_done;
                        job.total = total;
                    }
                    Ok(ReplaceMsg::Done {
                        replacements,
                        ms,
                        out,
                    }) => {
                        self.status = format!(
                            "Replaced {} occurrence{} → {} in {:.0} ms",
                            replacements,
                            if replacements == 1 { "" } else { "s" },
                            out.file_name().unwrap_or_default().to_string_lossy(),
                            ms
                        );
                        clear_replace = true;
                        break;
                    }
                    Ok(ReplaceMsg::Error(e)) => {
                        self.status = format!("Replace failed: {e}");
                        clear_replace = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        clear_replace = true;
                        break;
                    }
                }
            }
            if !clear_replace {
                ctx.request_repaint_after(Duration::from_millis(80));
            }
        }
        if clear_replace {
            self.replace_job = None;
        }
    }

    fn goto_hit(&mut self, idx: usize, _ctx: &egui::Context) {
        if idx >= self.hits.len() {
            return;
        }
        let h = self.hits[idx];
        self.current_hit = Some(idx);
        self.caret = h.offset + h.len;
        self.anchor = h.offset;
        self.desired_col = None;
        let loc = self.lidx.locate(&self.doc, h.offset);
        self.ensure_visible(loc.line);
    }

    fn find_next(&mut self, ctx: &egui::Context, forward: bool) {
        if self.hits.is_empty() {
            if self.search_job.is_none() {
                self.start_search(ctx);
            }
            return;
        }
        let anchor_off = self.caret.min(self.anchor);
        let next = if forward {
            self.hits.iter().position(|h| h.offset > anchor_off)
        } else {
            self.hits
                .iter()
                .rposition(|h| h.offset < anchor_off)
        };
        let idx = match next {
            Some(i) => i,
            None => {
                if self.opt_wrap {
                    if forward {
                        0
                    } else {
                        self.hits.len() - 1
                    }
                } else {
                    self.status = "No more matches".into();
                    return;
                }
            }
        };
        self.goto_hit(idx, ctx);
    }

    fn replace_one(&mut self, ctx: &egui::Context) {
        // If the current selection is exactly a match, replace it, else find next.
        if let Some(ci) = self.current_hit {
            if ci < self.hits.len() {
                let h = self.hits[ci];
                let (s, e) = self.sel_range();
                if s == h.offset && e == h.offset + h.len {
                    let rep = self.replace_text.clone();
                    self.replace_selection(rep.as_bytes());
                    self.status = "Replaced 1".into();
                    // hits are now stale; re-run search from here
                    self.start_search(ctx);
                    return;
                }
            }
        }
        self.find_next(ctx, true);
    }

    fn replace_all_memory(&mut self, ctx: &egui::Context) {
        if self.hits.is_empty() {
            self.status = "Run Find first".into();
            return;
        }
        if self
            .search_job
            .as_ref()
            .map(|j| j.running)
            .unwrap_or(false)
        {
            self.status = "Wait for search to finish".into();
            return;
        }
        let rep = self.replace_text.clone().into_bytes();
        let hits = self.hits.clone();
        self.push_undo();
        // Apply back-to-front so earlier offsets stay valid.
        let mut n = 0u64;
        for h in hits.iter().rev() {
            self.doc.edit(h.offset, h.offset + h.len, &rep);
            n += 1;
        }
        let first = hits.first().map(|h| h.offset).unwrap_or(0);
        self.caret = first;
        self.anchor = first;
        self.desired_col = None;
        self.lidx.notify_edit(0, self.doc.snapshot());
        self.hits.clear();
        self.current_hit = None;
        self.search_job = None;
        self.status = format!("Replaced {n} occurrence{}", if n == 1 { "" } else { "s" });
        let _ = ctx;
    }

    fn replace_all_to_file(&mut self, ctx: &egui::Context) {
        if self.find_text.is_empty() {
            return;
        }
        let out = match rfd::FileDialog::new()
            .set_file_name("replaced.txt")
            .save_file()
        {
            Some(p) => p,
            None => return,
        };
        let (tx, rx) = channel();
        let handle = replace::start(
            self.doc.snapshot(),
            self.opts(),
            self.replace_text.clone().into_bytes(),
            out,
            tx,
            ctx.clone(),
        );
        self.replace_job = Some(ReplaceJob {
            rx,
            handle,
            bytes_done: 0,
            total: self.doc.len() as u64,
        });
        self.status = "Streaming replace…".into();
    }

    // ---- navigation -----------------------------------------------------

    fn ensure_visible(&mut self, line: u64) {
        let rows = self.visible_rows.max(1) as f64;
        let l = line as f64;
        if l < self.top_line {
            self.top_line = l;
        } else if l >= self.top_line + rows - 1.0 {
            self.top_line = (l - rows + 2.0).max(0.0);
        }
        self.clamp_scroll();
    }

    fn max_top_line(&self) -> f64 {
        (self.lidx.total_lines().saturating_sub(1)) as f64
    }

    fn clamp_scroll(&mut self) {
        if self.top_line < 0.0 {
            self.top_line = 0.0;
        }
        let mx = self.max_top_line();
        if self.top_line > mx {
            self.top_line = mx;
        }
        if self.left_col < 0.0 {
            self.left_col = 0.0;
        }
    }

    fn move_caret(&mut self, to: usize, extend: bool) {
        self.caret = to.min(self.doc.len());
        if !extend {
            self.anchor = self.caret;
        }
        self.blink_phase = 0.0;
        let loc = self.lidx.locate(&self.doc, self.caret);
        self.ensure_visible(loc.line);
    }

    fn move_vertical(&mut self, down: bool, extend: bool, rows: usize) {
        let loc = self.lidx.locate(&self.doc, self.caret);
        let col = self
            .desired_col
            .unwrap_or_else(|| visual_col(&self.doc, loc.line_start, self.caret));
        let target_line = if down {
            loc.line + rows as u64
        } else {
            loc.line.saturating_sub(rows as u64)
        };
        let tstart = self.lidx.line_start(&self.doc, target_line);
        let tend = self.lidx.line_end(&self.doc, tstart);
        let to = offset_for_visual_col(&self.doc, tstart, tend, col);
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
        self.desired_col = Some(col);
        self.blink_phase = 0.0;
        self.ensure_visible(target_line);
    }

    // ---- input ---------------------------------------------------------

    fn handle_keys(&mut self, ui: &mut egui::Ui, rect: Rect, vlines: &[VLine]) {
        let ctx = ui.ctx().clone();
        let editable_focus = ctx.memory(|m| m.focused()).is_none();

        // Global shortcuts (work regardless of focus). `consume_key` removes the
        // event so no other widget also acts on it.
        macro_rules! sc {
            ($mods:expr, $key:expr) => {
                ui.input_mut(|i| i.consume_key($mods, $key))
            };
        }
        if sc!(Modifiers::COMMAND, Key::O) {
            self.open_dialog(&ctx);
        }
        if sc!(Modifiers::COMMAND, Key::S) {
            self.save(&ctx, false);
        }
        if sc!(Modifiers::COMMAND | Modifiers::SHIFT, Key::S) {
            self.save(&ctx, true);
        }
        if sc!(Modifiers::COMMAND, Key::N) {
            self.doc = Document::new_empty();
            self.lidx = LineIndex::new();
            self.lidx.start(self.doc.snapshot(), ctx.clone());
            self.caret = 0;
            self.anchor = 0;
            self.top_line = 0.0;
            self.undo.clear();
            self.redo.clear();
            self.invalidate_search();
            self.status = "New file".into();
        }
        if sc!(Modifiers::COMMAND, Key::F) {
            self.show_find = true;
            self.replace_mode = false;
            self.focus_find = true;
            if self.has_sel() {
                let s = self.selection_string();
                if !s.contains('\n') && !s.is_empty() {
                    self.find_text = s;
                }
            }
        }
        if sc!(Modifiers::COMMAND, Key::H) {
            self.show_find = true;
            self.replace_mode = true;
            self.focus_find = true;
        }
        if sc!(Modifiers::COMMAND, Key::G) {
            self.show_goto = true;
            self.focus_goto = true;
        }
        if sc!(Modifiers::COMMAND, Key::A) {
            self.anchor = 0;
            self.caret = self.doc.len();
            self.blink_phase = 0.0;
        }
        if sc!(Modifiers::COMMAND, Key::Z) {
            self.do_undo();
        }
        if sc!(Modifiers::COMMAND, Key::Y) || sc!(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) {
            self.do_redo();
        }
        if sc!(Modifiers::COMMAND, Key::C) {
            let s = self.selection_string();
            if !s.is_empty() {
                ui.ctx().copy_text(s);
            }
        }
        if sc!(Modifiers::COMMAND, Key::X) {
            let s = self.selection_string();
            if !s.is_empty() {
                ui.ctx().copy_text(s);
                self.replace_selection(b"");
            }
        }
        let (f3, f3_shift) = ui.input_mut(|i| {
            let shift = i.modifiers.shift;
            (i.consume_key(Modifiers::NONE, Key::F3)
                || i.consume_key(Modifiers::SHIFT, Key::F3), shift)
        });
        if f3 {
            self.find_next(&ctx, !f3_shift);
            return;
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            if self.show_find || self.show_goto {
                self.show_find = false;
                self.show_goto = false;
            }
        }

        // Text-grid editing keys only when no text field is focused.
        if !editable_focus {
            return;
        }
        let _ = (rect, vlines);
        let events = ui.input(|i| i.events.clone());
        // Stop egui from using these for focus navigation / default actions;
        // `events` above is already a snapshot, so our handler still sees them.
        ui.input_mut(|i| {
            for k in [
                Key::Tab,
                Key::Enter,
                Key::ArrowUp,
                Key::ArrowDown,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::Home,
                Key::End,
                Key::PageUp,
                Key::PageDown,
            ] {
                for m in [
                    Modifiers::NONE,
                    Modifiers::SHIFT,
                    Modifiers::COMMAND,
                    Modifiers::COMMAND | Modifiers::SHIFT,
                ] {
                    i.consume_key(m, k);
                }
            }
        });
        let rows = self.visible_rows;
        for ev in events {
            match ev {
                egui::Event::Text(t) if !t.is_empty() => {
                    // Filter control chars; Enter/Tab handled via Key events.
                    let filtered: String = t.chars().filter(|c| !c.is_control()).collect();
                    if !filtered.is_empty() {
                        self.replace_selection(filtered.as_bytes());
                    }
                }
                egui::Event::Paste(t) => {
                    let norm = t.replace("\r\n", "\n");
                    self.replace_selection(norm.as_bytes());
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let ext = modifiers.shift;
                    match key {
                        Key::ArrowLeft => {
                            if modifiers.command {
                                let to = word_left(&self.doc, self.caret);
                                self.move_caret(to, ext);
                            } else if self.has_sel() && !ext {
                                self.move_caret(self.sel_range().0, false);
                            } else {
                                let to = char_boundary_before(&self.doc, self.caret);
                                self.move_caret(to, ext);
                            }
                            self.desired_col = None;
                        }
                        Key::ArrowRight => {
                            if modifiers.command {
                                let to = word_right(&self.doc, self.caret);
                                self.move_caret(to, ext);
                            } else if self.has_sel() && !ext {
                                self.move_caret(self.sel_range().1, false);
                            } else {
                                let to = char_boundary_after(&self.doc, self.caret);
                                self.move_caret(to, ext);
                            }
                            self.desired_col = None;
                        }
                        Key::ArrowUp => {
                            if modifiers.command {
                                self.top_line -= 1.0;
                                self.clamp_scroll();
                            } else {
                                self.move_vertical(false, ext, 1);
                            }
                        }
                        Key::ArrowDown => {
                            if modifiers.command {
                                self.top_line += 1.0;
                                self.clamp_scroll();
                            } else {
                                self.move_vertical(true, ext, 1);
                            }
                        }
                        Key::Home => {
                            if modifiers.command {
                                self.top_line = 0.0;
                                self.move_caret(0, ext);
                            } else {
                                let loc = self.lidx.locate(&self.doc, self.caret);
                                self.move_caret(loc.line_start, ext);
                            }
                            self.desired_col = None;
                        }
                        Key::End => {
                            if modifiers.command {
                                if self.lidx.is_complete() {
                                    let end = self.doc.len();
                                    self.move_caret(end, ext);
                                    self.top_line = self.max_top_line();
                                } else {
                                    self.status = "Index still building — jump to end shortly".into();
                                }
                            } else {
                                let loc = self.lidx.locate(&self.doc, self.caret);
                                let e = self.lidx.line_end(&self.doc, loc.line_start);
                                self.move_caret(e, ext);
                            }
                            self.desired_col = None;
                        }
                        Key::PageUp => {
                            self.top_line -= (rows.saturating_sub(1)) as f64;
                            self.clamp_scroll();
                            self.move_vertical(false, ext, rows.saturating_sub(1).max(1));
                        }
                        Key::PageDown => {
                            self.top_line += (rows.saturating_sub(1)) as f64;
                            self.clamp_scroll();
                            self.move_vertical(true, ext, rows.saturating_sub(1).max(1));
                        }
                        Key::Backspace => self.backspace(),
                        Key::Delete => self.delete_fwd(),
                        Key::Enter => self.replace_selection(b"\n"),
                        Key::Tab => self.replace_selection(b"\t"),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_mouse(&mut self, ui: &mut egui::Ui, rect: Rect, resp: &egui::Response, m: &Metrics, vlines: &[VLine]) {
        // wheel / trackpad scroll
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.y != 0.0 && rect.contains(ui.input(|i| i.pointer.hover_pos().unwrap_or(rect.center()))) {
            self.top_line -= (scroll.y / m.row_h) as f64;
            self.clamp_scroll();
        }
        if scroll.x != 0.0 {
            self.left_col -= scroll.x / m.col_w;
            if self.left_col < 0.0 {
                self.left_col = 0.0;
            }
        }

        let pos = resp.interact_pointer_pos();
        if let Some(p) = pos {
            if resp.clicked() || resp.drag_started() {
                if let Some(off) = self.pos_to_offset(p, m, vlines) {
                    let extend = ui.input(|i| i.modifiers.shift) || resp.dragged();
                    self.caret = off;
                    if !extend && !resp.dragged() {
                        self.anchor = off;
                    }
                    if resp.drag_started() && !ui.input(|i| i.modifiers.shift) {
                        self.anchor = off;
                    }
                    self.desired_col = None;
                    self.blink_phase = 0.0;
                }
            } else if resp.dragged() {
                if let Some(off) = self.pos_to_offset(p, m, vlines) {
                    self.caret = off;
                    self.desired_col = None;
                    // auto-scroll while dragging past edges
                    if p.y < rect.top() + m.row_h {
                        self.top_line -= 1.0;
                    } else if p.y > rect.bottom() - m.row_h {
                        self.top_line += 1.0;
                    }
                    self.clamp_scroll();
                }
            }
        }

        if resp.double_clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                if let Some(off) = self.pos_to_offset(p, m, vlines) {
                    self.anchor = word_left(&self.doc, off);
                    self.caret = word_right(&self.doc, off);
                }
            }
        }
    }

    fn pos_to_offset(&self, p: Pos2, m: &Metrics, vlines: &[VLine]) -> Option<usize> {
        if vlines.is_empty() {
            return Some(0);
        }
        let row = ((p.y - m.text_y0 + m.frac_px) / m.row_h).floor();
        let row = row.max(0.0) as usize;
        let row = row.min(vlines.len() - 1);
        let vl = &vlines[row];
        let col_f = (p.x - m.text_x0) / m.col_w + self.left_col;
        let col = col_f.round().max(0.0) as usize;
        Some(vl.off_of_col(col))
    }

    // ---- painting ----------------------------------------------------

    fn paint(&self, ui: &egui::Ui, rect: Rect, m: &Metrics, vlines: &[VLine]) {
        let painter = ui.painter_at(rect);
        let t = &self.theme;
        painter.rect_filled(rect, 0.0, t.bg);

        let gutter_rect = Rect::from_min_size(rect.min, Vec2::new(m.gutter_w, rect.height()));
        painter.rect_filled(gutter_rect, 0.0, t.gutter_bg);

        let font = FontId::monospace(self.font_size);
        let first_line = self.top_line.floor() as u64;
        let frac_px = m.frac_px;

        let (sel_s, sel_e) = self.sel_range();

        for (i, vl) in vlines.iter().enumerate() {
            let y = m.text_y0 + i as f32 * m.row_h - frac_px;
            if y > rect.bottom() {
                break;
            }
            let line_no = first_line + i as u64;

            // line number
            painter.text(
                Pos2::new(rect.left() + m.gutter_w - 6.0, y),
                Align2::RIGHT_TOP,
                format!("{}", line_no + 1),
                font.clone(),
                t.gutter_fg,
            );

            let x_base = m.text_x0 - self.left_col * m.col_w;

            // selection
            if sel_e > sel_s && sel_s < vl.next_line && sel_e > vl.start {
                let a = sel_s.max(vl.start);
                let b = sel_e.min(vl.next_line);
                let ca = vl.col_of(a.min(vl.content_end));
                let x0 = x_base + ca as f32 * m.col_w;
                let x1 = if b > vl.content_end {
                    x_base + (vl.cols() as f32 + 1.0) * m.col_w
                } else {
                    x_base + vl.col_of(b) as f32 * m.col_w
                };
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1.max(x0 + 1.0), y + m.row_h)),
                    0.0,
                    t.sel,
                );
            }

            // search hits on this line
            for (hi, h) in self.hits.iter().enumerate() {
                if h.offset >= vl.next_line || h.offset + h.len <= vl.start {
                    continue;
                }
                let a = h.offset.max(vl.start);
                let b = (h.offset + h.len).min(vl.content_end);
                let x0 = x_base + vl.col_of(a) as f32 * m.col_w;
                let x1 = x_base + vl.col_of(b) as f32 * m.col_w;
                let col = if Some(hi) == self.current_hit {
                    t.hit_current
                } else {
                    t.hit
                };
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1.max(x0 + 2.0), y + m.row_h)),
                    0.0,
                    col,
                );
            }

            // text
            painter.text(
                Pos2::new(x_base, y),
                Align2::LEFT_TOP,
                &vl.disp,
                font.clone(),
                t.fg,
            );

            // caret
            if self.caret >= vl.start && self.caret <= vl.content_end && self.blink_phase < 0.5 {
                let cx = x_base + vl.col_of(self.caret) as f32 * m.col_w;
                painter.line_segment(
                    [Pos2::new(cx, y), Pos2::new(cx, y + m.row_h)],
                    Stroke::new(1.5_f32, t.caret),
                );
            }
        }

        self.paint_scrollbar(&painter, rect, m);
    }

    fn paint_scrollbar(&self, painter: &egui::Painter, rect: Rect, m: &Metrics) {
        let t = &self.theme;
        let total = self.lidx.total_lines().max(1) as f64;
        let sb = Rect::from_min_max(
            Pos2::new(rect.right() - m.sb_w, rect.top()),
            Pos2::new(rect.right(), rect.bottom()),
        );
        painter.rect_filled(sb, 0.0, t.scrollbar_bg);
        let frac_vis = (self.visible_rows as f64 / total).min(1.0);
        let handle_h = (sb.height() as f64 * frac_vis).max(24.0) as f32;
        let denom = (total - 1.0).max(1.0);
        let ty = sb.top() + ((sb.height() - handle_h) as f64 * (self.top_line / denom)) as f32;
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(sb.left() + 2.0, ty), Vec2::new(m.sb_w - 4.0, handle_h)),
            3.0,
            t.scrollbar,
        );
    }
}

struct Metrics {
    row_h: f32,
    col_w: f32,
    gutter_w: f32,
    text_x0: f32,
    text_y0: f32,
    /// pixels the first visible row is scrolled up by (fractional scroll)
    frac_px: f32,
    sb_w: f32,
}

impl eframe::App for Gred {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_async(ctx);

        // caret blink
        self.blink_phase += ctx.input(|i| i.stable_dt).min(0.1);
        if self.blink_phase >= 1.0 {
            self.blink_phase = 0.0;
        }
        ctx.request_repaint_after(Duration::from_millis(500));

        // window title
        let name = self
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".into());
        let title = format!("{}{} — gred", if self.doc.dirty { "*" } else { "" }, name);
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        self.menu_bar(ctx);
        if self.show_find {
            self.find_bar(ctx);
        }
        self.status_bar(ctx);
        if self.show_goto {
            self.goto_window(ctx);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(self.theme.bg))
            .show(ctx, |ui| {
                let full = ui.available_rect_before_wrap();
                let (rect, resp) =
                    ui.allocate_exact_size(full.size(), egui::Sense::click_and_drag());

                let font = FontId::monospace(self.font_size);
                let row_h = ui.fonts(|f| f.row_height(&font));
                let col_w = ui.fonts(|f| f.glyph_width(&font, 'M')).max(1.0);

                let digits = ((self.lidx.total_lines().max(1)) as f64).log10().floor() as usize + 1;
                let gutter_w = col_w * (digits.max(4) as f32 + 2.0);
                let sb_w = 14.0;
                self.clamp_scroll();
                let m = Metrics {
                    row_h,
                    col_w,
                    gutter_w,
                    text_x0: rect.left() + gutter_w + 4.0,
                    text_y0: rect.top() + 2.0,
                    frac_px: (self.top_line.fract() as f32) * row_h,
                    sb_w,
                };

                self.visible_rows = ((rect.height() - 4.0) / row_h).max(1.0) as usize;

                // Build the visible lines from the current scroll position.
                let first_line = self.top_line.floor() as u64;
                let start_off = self.lidx.line_start(&self.doc, first_line);
                let mut vlines =
                    build_vlines(&self.doc, start_off, self.visible_rows + 2, MAX_LINE_CHARS);

                // We deliberately do NOT take egui keyboard focus for the text
                // grid: editing keys are read straight from the event stream and
                // are active whenever no find/goto field holds focus.
                let doc_len_before = self.doc.len();
                self.handle_mouse(ui, rect, &resp, &m, &vlines);
                self.handle_keys(ui, rect, &vlines);

                // Rebuild if the document or scroll changed under us.
                let first_line2 = self.top_line.floor() as u64;
                if self.doc.len() != doc_len_before || first_line2 != first_line {
                    let so = self.lidx.line_start(&self.doc, first_line2);
                    vlines = build_vlines(&self.doc, so, self.visible_rows + 2, MAX_LINE_CHARS);
                }

                self.paint(ui, rect, &m, &vlines);
            });
    }
}

// ---------------------------------------------------------------------------
// UI panels
// ---------------------------------------------------------------------------
impl Gred {
    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New\tCtrl+N").clicked() {
                        self.doc = Document::new_empty();
                        self.lidx = LineIndex::new();
                        self.lidx.start(self.doc.snapshot(), ctx.clone());
                        self.caret = 0;
                        self.anchor = 0;
                        self.top_line = 0.0;
                        self.undo.clear();
                        self.redo.clear();
                        self.invalidate_search();
                        ui.close_menu();
                    }
                    if ui.button("Open…\tCtrl+O").clicked() {
                        self.open_dialog(ctx);
                        ui.close_menu();
                    }
                    if ui.button("Save\tCtrl+S").clicked() {
                        self.save(ctx, false);
                        ui.close_menu();
                    }
                    if ui.button("Save As…\tCtrl+Shift+S").clicked() {
                        self.save(ctx, true);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui.button("Undo\tCtrl+Z").clicked() {
                        self.do_undo();
                        ui.close_menu();
                    }
                    if ui.button("Redo\tCtrl+Y").clicked() {
                        self.do_redo();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Cut\tCtrl+X").clicked() {
                        let s = self.selection_string();
                        if !s.is_empty() {
                            ctx.copy_text(s);
                            self.replace_selection(b"");
                        }
                        ui.close_menu();
                    }
                    if ui.button("Copy\tCtrl+C").clicked() {
                        let s = self.selection_string();
                        if !s.is_empty() {
                            ctx.copy_text(s);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Select All\tCtrl+A").clicked() {
                        self.anchor = 0;
                        self.caret = self.doc.len();
                        ui.close_menu();
                    }
                });
                ui.menu_button("Search", |ui| {
                    if ui.button("Find…\tCtrl+F").clicked() {
                        self.show_find = true;
                        self.replace_mode = false;
                        self.focus_find = true;
                        ui.close_menu();
                    }
                    if ui.button("Replace…\tCtrl+H").clicked() {
                        self.show_find = true;
                        self.replace_mode = true;
                        self.focus_find = true;
                        ui.close_menu();
                    }
                    if ui.button("Find Next\tF3").clicked() {
                        self.find_next(ctx, true);
                        ui.close_menu();
                    }
                    if ui.button("Find Previous\tShift+F3").clicked() {
                        self.find_next(ctx, false);
                        ui.close_menu();
                    }
                    if ui.button("Go to Line…\tCtrl+G").clicked() {
                        self.show_goto = true;
                        self.focus_goto = true;
                        ui.close_menu();
                    }
                });
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("{}px", self.font_size as i32))
                        .weak(),
                );
                if ui.small_button("A-").clicked() {
                    self.font_size = (self.font_size - 1.0).max(8.0);
                }
                if ui.small_button("A+").clicked() {
                    self.font_size = (self.font_size + 1.0).min(40.0);
                }
            });
        });
    }

    fn find_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("find").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Find:");
                let te = ui.add(
                    egui::TextEdit::singleline(&mut self.find_text)
                        .desired_width(240.0)
                        .hint_text("text or /regex/"),
                );
                if self.focus_find {
                    te.request_focus();
                    self.focus_find = false;
                }
                let submit = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if submit {
                    self.start_search(ctx);
                    ui.memory_mut(|mm| mm.request_focus(te.id));
                }
                if ui.button("◀").on_hover_text("Previous (Shift+F3)").clicked() {
                    self.find_next(ctx, false);
                }
                if ui.button("▶").on_hover_text("Next (F3)").clicked() {
                    self.find_next(ctx, true);
                }
                if ui.button("Find All").clicked() {
                    self.start_search(ctx);
                }
                ui.separator();
                ui.checkbox(&mut self.opt_case, "Aa").on_hover_text("Match case");
                let word = ui.add_enabled(
                    !self.opt_regex,
                    egui::Checkbox::new(&mut self.opt_word, "W"),
                );
                word.on_hover_text("Whole word (disabled for regex)");
                ui.checkbox(&mut self.opt_regex, ".*").on_hover_text("Regular expression");
                ui.checkbox(&mut self.opt_wrap, "wrap").on_hover_text("Wrap around");

                let count = self.hits.len();
                let running = self
                    .search_job
                    .as_ref()
                    .map(|j| j.running)
                    .unwrap_or(false);
                let pos = self
                    .current_hit
                    .map(|i| format!("{}/", i + 1))
                    .unwrap_or_default();
                ui.label(
                    egui::RichText::new(if running {
                        format!("{}{}…", pos, count)
                    } else {
                        format!("{}{}", pos, count)
                    })
                    .weak(),
                );
                if ui.button("✕").clicked() {
                    self.show_find = false;
                }
            });

            if self.replace_mode {
                ui.horizontal_wrapped(|ui| {
                    ui.label("With:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.replace_text)
                            .desired_width(240.0),
                    );
                    if ui.button("Replace").clicked() {
                        self.replace_one(ctx);
                    }
                    if ui.button("Replace All").on_hover_text("In memory (undoable)").clicked() {
                        self.replace_all_memory(ctx);
                    }
                    if ui
                        .button("Replace All → File")
                        .on_hover_text("Stream to a new file; works beyond RAM")
                        .clicked()
                    {
                        self.replace_all_to_file(ctx);
                    }
                    if let Some(job) = &self.replace_job {
                        let frac = if job.total > 0 {
                            job.bytes_done as f32 / job.total as f32
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(frac).desired_width(120.0));
                        if ui.button("Cancel").clicked() {
                            job.handle.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });
    }

    fn goto_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        egui::Window::new("Go to line")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let te = ui.add(egui::TextEdit::singleline(&mut self.goto_text).desired_width(120.0));
                if self.focus_goto {
                    te.request_focus();
                    self.focus_goto = false;
                }
                let go = ui.button("Go").clicked()
                    || (te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
                if go {
                    if let Ok(n) = self.goto_text.trim().parse::<u64>() {
                        let line = n.saturating_sub(1);
                        let off = self.lidx.line_start(&self.doc, line);
                        self.caret = off;
                        self.anchor = off;
                        self.desired_col = None;
                        self.top_line = line as f64;
                        self.clamp_scroll();
                        self.ensure_visible(line);
                    }
                    self.show_goto = false;
                }
            });
        if !open {
            self.show_goto = false;
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let loc = self.lidx.locate(&self.doc, self.caret);
                let col = visual_col(&self.doc, loc.line_start, self.caret) + 1;
                ui.label(format!("Ln {}, Col {}", loc.line + 1, col));
                ui.separator();
                let total = self.lidx.total_lines();
                if self.lidx.is_complete() {
                    ui.label(format!("{} lines", total));
                } else {
                    ui.label(format!(
                        "{}+ lines (indexing {:.0}%)",
                        total,
                        self.lidx.scanned_fraction() * 100.0
                    ));
                }
                ui.separator();
                ui.label(human_bytes(self.doc.len() as u64));
                if let Some(ms) = self.open_ms {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!("open→paint {:.0} ms", ms)).weak(),
                    );
                }
                ui.separator();
                ui.label(egui::RichText::new(&self.status).weak());
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------
fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", n)
    } else {
        format!("{:.1} {}", f, U[i])
    }
}

fn utf8_width(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn char_boundary_before(doc: &Document, off: usize) -> usize {
    if off == 0 {
        return 0;
    }
    let s = off.saturating_sub(4);
    let mut b = [0u8; 4];
    let n = doc.read_into(s, &mut b[..(off - s)]);
    if n == 0 {
        return off - 1;
    }
    let slice = &b[..n];
    let mut i = slice.len() - 1;
    while i > 0 && (slice[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    s + i
}

fn char_boundary_after(doc: &Document, off: usize) -> usize {
    let len = doc.len();
    if off >= len {
        return len;
    }
    let mut b = [0u8; 1];
    let n = doc.read_into(off, &mut b);
    if n == 0 {
        return len;
    }
    (off + utf8_width(b[0])).min(len)
}

fn class(b: u8) -> u8 {
    if b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80 {
        2
    } else if b == b' ' || b == b'\t' {
        0
    } else {
        1
    }
}

fn word_left(doc: &Document, off: usize) -> usize {
    let mut i = off;
    let mut buf = [0u8; 1];
    // skip whitespace
    while i > 0 {
        doc.read_into(i - 1, &mut buf);
        if class(buf[0]) != 0 {
            break;
        }
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    doc.read_into(i - 1, &mut buf);
    let c = class(buf[0]);
    while i > 0 {
        doc.read_into(i - 1, &mut buf);
        if class(buf[0]) != c || buf[0] == b'\n' {
            break;
        }
        i -= 1;
    }
    i
}

fn word_right(doc: &Document, off: usize) -> usize {
    let len = doc.len();
    let mut i = off;
    let mut buf = [0u8; 1];
    if i >= len {
        return len;
    }
    doc.read_into(i, &mut buf);
    let c = class(buf[0]);
    while i < len {
        doc.read_into(i, &mut buf);
        if class(buf[0]) != c || buf[0] == b'\n' {
            break;
        }
        i += 1;
    }
    while i < len {
        doc.read_into(i, &mut buf);
        if class(buf[0]) != 0 || buf[0] == b'\n' {
            break;
        }
        i += 1;
    }
    i
}

/// Visual column (tab-expanded char count) between `line_start` and `off`.
const MAX_SCAN_LINE: usize = 1 << 20;

fn visual_col(doc: &Document, line_start: usize, off: usize) -> usize {
    if off <= line_start {
        return 0;
    }
    let mut buf = vec![0u8; (off - line_start).min(MAX_SCAN_LINE)];
    let n = doc.read_into(line_start, &mut buf);
    let mut col = 0usize;
    let mut i = 0usize;
    while i < n {
        let b = buf[i];
        if b == b'\t' {
            col += TAB_W - (col % TAB_W);
            i += 1;
        } else if b == b'\n' {
            break;
        } else {
            let w = utf8_width(b);
            col += 1;
            i += w.max(1);
        }
    }
    col
}

/// Doc offset within `[line_start, line_end]` closest to visual column `col`.
fn offset_for_visual_col(doc: &Document, line_start: usize, line_end: usize, col: usize) -> usize {
    if line_end <= line_start {
        return line_start;
    }
    let mut buf = vec![0u8; (line_end - line_start).min(MAX_SCAN_LINE)];
    let n = doc.read_into(line_start, &mut buf);
    let mut cur = 0usize;
    let mut i = 0usize;
    while i < n {
        if cur >= col {
            return line_start + i;
        }
        let b = buf[i];
        if b == b'\n' {
            return line_start + i;
        }
        if b == b'\t' {
            cur += TAB_W - (cur % TAB_W);
            i += 1;
        } else {
            cur += 1;
            i += utf8_width(b).max(1);
        }
    }
    line_start + n
}

/// Build up to `rows` visible lines starting at document offset `start_off`.
fn build_vlines(doc: &Document, start_off: usize, rows: usize, max_chars: usize) -> Vec<VLine> {
    let total = doc.len();
    let mut out: Vec<VLine> = Vec::with_capacity(rows);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut raw: Vec<u8> = Vec::with_capacity(256);
    let mut off = start_off;
    let mut line_start = start_off;

    loop {
        if out.len() >= rows {
            break;
        }
        if off >= total {
            let (disp, map) = render_line(&raw, line_start, max_chars);
            out.push(VLine {
                start: line_start,
                content_end: line_start + raw.len(),
                next_line: total,
                disp,
                map,
            });
            break;
        }
        let n = doc.read_into(off, &mut chunk);
        if n == 0 {
            let (disp, map) = render_line(&raw, line_start, max_chars);
            out.push(VLine {
                start: line_start,
                content_end: line_start + raw.len(),
                next_line: total,
                disp,
                map,
            });
            break;
        }
        let mut i = 0usize;
        while i < n {
            match memchr::memchr(b'\n', &chunk[i..n]) {
                Some(p) => {
                    if raw.len() < max_chars * 4 {
                        raw.extend_from_slice(&chunk[i..i + p]);
                    }
                    let content_end = line_start + raw.len();
                    let next_line = off + i + p + 1;
                    let (disp, map) = render_line(&raw, line_start, max_chars);
                    out.push(VLine {
                        start: line_start,
                        content_end,
                        next_line,
                        disp,
                        map,
                    });
                    raw.clear();
                    i += p + 1;
                    line_start = off + i;
                    if out.len() >= rows {
                        break;
                    }
                }
                None => {
                    if raw.len() < max_chars * 4 {
                        raw.extend_from_slice(&chunk[i..n]);
                    }
                    i = n;
                }
            }
        }
        off += n;
    }
    out
}

/// Render one raw line (no trailing `\n`) to a display string plus a
/// display-char -> document-offset map. The map's final entry is the offset
/// just past the line content.
fn render_line(raw: &[u8], start: usize, max_chars: usize) -> (String, Vec<usize>) {
    let text = String::from_utf8_lossy(raw);
    let exact = matches!(text, std::borrow::Cow::Borrowed(_));
    let mut disp = String::with_capacity(raw.len().min(max_chars) + 1);
    let mut map: Vec<usize> = Vec::with_capacity(raw.len().min(max_chars) + 2);
    let mut col = 0usize;
    let mut chars = 0usize;

    for (bi, ch) in text.char_indices() {
        if chars >= max_chars {
            disp.push('…');
            map.push(start + raw.len());
            return (disp, map);
        }
        let doc_b = start + if exact { bi } else { bi.min(raw.len()) };
        match ch {
            '\t' => {
                let spaces = TAB_W - (col % TAB_W);
                for _ in 0..spaces {
                    disp.push(' ');
                    map.push(doc_b);
                    col += 1;
                }
            }
            '\r' => {
                disp.push(' ');
                map.push(doc_b);
                col += 1;
            }
            c if (c as u32) < 0x20 => {
                disp.push('·');
                map.push(doc_b);
                col += 1;
            }
            c => {
                disp.push(c);
                map.push(doc_b);
                col += 1;
            }
        }
        chars += 1;
    }
    map.push(start + raw.len());
    (disp, map)
}
