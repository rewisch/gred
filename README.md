# gred

A very fast, very lean text **viewer/editor for huge files** on Windows, in Rust.

Not a general-purpose editor and not a Notepad++ clone. It does one thing well:
open enormous text files instantly, move around them, search, replace, make small
edits, and save — with **large files feeling small**. It also does the ordinary
Notepad things (type, select, cut/copy/paste, undo/redo) on normal files.

The design contract (from `Plan.txt`):

* **Never load the whole file.** The original is memory-mapped; edits layer on
  top as a piece table. Reads walk pieces; the OS pages in only what you look at.
* **Open → first visible text in well under 250 ms**, even for a 50 GB file.
* **No operation blocks because of file size.** Line indexing, scans and metadata
  happen on background threads, throttled, with the UI always winning the CPU.
* **Memory does not grow with the file.** Our owned memory stays in the low
  double-digit MB range at 50 GB.
* Every design choice is checked against "what happens at 50 GB?"

## Features

| | |
|---|---|
| Open / view | mmap open (O(1)), viewport-only rendering, smooth pixel scrolling, sparse background line index, go-to-line |
| Edit | insert / delete / overtype, Enter, Tab, word-wise cursor, click & drag select, double-click word select, Select All |
| Clipboard | Cut / Copy / Paste (Ctrl+X / C / V) |
| Undo | unlimited-ish undo/redo via cheap piece-table checkpoints (Ctrl+Z / Ctrl+Y) |
| Find | streaming search on the ripgrep libraries — match case, whole word, regex toggle, wrap-around, direction; hits stream in, first hit is reachable immediately |
| Replace | replace one; **Replace All (in memory, undoable)**; **Replace All → new file** which *streams* and works on files larger than RAM |
| Save | streamed sequentially to a temp file then atomically swapped; the document is re-mapped afterwards so memory drops back to baseline |

### Keyboard

```
Ctrl+O  open              Ctrl+F  find            F3 / Shift+F3  next / prev match
Ctrl+S  save              Ctrl+H  replace         Ctrl+G  go to line
Ctrl+Shift+S  save as     Ctrl+Z / Ctrl+Y  undo / redo
Ctrl+N  new               Ctrl+A  select all      Ctrl+←/→  word left / right
Home / End  line ends     Ctrl+Home / End  file ends   PgUp / PgDn  page
```

## Architecture

```
src/
  document.rs   file access + piece table + streaming reader + save
  lineindex.rs  sparse line anchors, built by a throttled background worker
  search.rs     streaming search (grep-regex / grep-searcher), channel of hits
  replace.rs    line-streaming Replace-All into a new file
  app.rs        egui UI: virtualized text grid, cursor/selection, editing
  bench.rs      headless benchmark / self-test harness
```

* **document** — The original file is `mmap`'d as one `Orig` piece. `edit()`
  splits pieces around the range and inserts an `Add` piece pointing into an
  append-only buffer; sequential typing coalesces into a single piece. Undo is a
  `Checkpoint` (just `Arc`s), so it is O(1) to take and to restore. `save()`
  streams every piece to `<file>.gred.tmp`, swaps it in, then re-maps.
* **lineindex** — A worker thread *streams* the file (plain `ReadFile`, not mmap,
  so the working set stays flat) counting `\n`, storing the byte offset of every
  4096th line. Locating line _N_ = jump to the nearest earlier anchor, scan
  forward ≤ 4096 lines. Until the scan reaches a line, the scrollbar just doesn't
  extend that far — nothing blocks. An edit keeps the anchors before it and
  rescans forward.
* **search** — `grep-regex` builds the matcher (plain text is escaped; whole-word
  uses grep's `word()`; regex disables whole-word per the plan). Files ≤ 256 MB
  are searched zero-copy over the mmap; larger files stream. Every match is sent
  down a channel as it is found and the UI can jump to it right away. Capped at
  500k hits.
* **app** — Each frame builds only the ~50 visible lines (`build_vlines`), maps
  every displayed character back to a document byte offset (tabs expanded, control
  chars shown), and paints line numbers, selection, match highlights, caret. The
  cursor is stored as a single document byte offset.

### Known MVP limitations

* Streaming *Replace All → file* is line-oriented: a match that spans a newline is
  not replaced (in-memory Replace All has no such limit).
* Editing a multi-GB file briefly shrinks the scrollbar while the tail past the
  edit re-indexes in the background (a few seconds, throttled). The viewport near
  the edit is served immediately from the nearest anchor.
* Very long lines are rendered/truncated at 4000 display chars.
* Tabs are shown as 4 spaces.

## Benchmarks

Synthetic files (`gred --gen <file> <size_gb>`), Windows Server 2022, 4 vCPU SSD.
Measured with `gred --bench`; peak memory sampled by a PowerShell wrapper.

| file | `open()` | **open → first paint** | line index (bg) | search (full) | random-scroll latency | peak private / working set |
|---|---|---|---|---|---|---|
| 1 GB (20.8 M lines)  | 0.16 ms | **1.1 ms** | 1.0 s @ 1.0 GB/s | 316 ms, 41.5k hits, 3.2 GB/s | 132 µs avg / 0.6 ms worst | **5.8 MB** / 41 MB |
| 10 GB (207 M lines)  | 1.6 ms  | **3.7 ms** | 10.5 s @ 0.98 GB/s | 4.9 s, 415k hits, 2.1 GB/s | 1.2 ms avg / 8.5 ms worst | **24.8 MB** / 44 MB |

`open → first paint` is `open()` plus decoding the first screenful — the number
the plan caps at 250 ms. Line indexing and search are background/streaming; their
first useful result (first visible screen, first match) is available immediately.
50 GB was not run (disk), but every metric is flat or linear in file size and the
owned-memory figure extrapolates to ~30 MB.

## Building

Requires the Rust **`x86_64-pc-windows-gnu`** toolchain and a full **MinGW-w64**
(for `dlltool`/`as`, used to generate `raw-dylib` import libraries):

```
choco install rust mingw
```

`.cargo/config.toml` points the linker and `dlltool` at
`C:\ProgramData\mingw64\mingw64\bin`. Then:

```
cargo build --release
cargo test
```

### Running headless / over RDP

On a normal desktop with a GPU, just run `gred.exe` — nothing extra needed.

On a machine with no usable GPU driver (headless server, plain RDP session)
eframe's OpenGL context fails, and Mesa's default renderer path can even
segfault. Two steps:

1. Put Mesa3D's `opengl32.dll` + `libgallium_wgl.dll` next to `gred.exe`
   (the **MinGW** build from <https://github.com/pal1000/mesa-dist-win/releases> —
   the MSVC build needs a newer VC++ runtime).
2. Start with `--software` so gred forces the llvmpipe software renderer
   (`GALLIUM_DRIVER=llvmpipe`). Env var `GRED_SOFTWARE=1` does the same.

`gred-software-gl.cmd` and `run.ps1` do both for you. Rendering is still CPU-only
and less stable than a real display session — for heavy use, run gred on an
ordinary Windows desktop.

## Usage

```
gred [file]                 open the GUI (optionally on a file)
gred --software [file]       force Mesa llvmpipe software GL (headless / RDP)
gred --gen <file> <gb>       write a synthetic test file
gred --bench <file> [--search <pat>]   headless benchmark / self-test
```
