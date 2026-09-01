// gred — a very fast, very lean viewer/editor for huge text files.
//
// Design north star (see Plan.txt): never load the whole file; open-to-first-
// paint under ~250 ms even for 50 GB; nothing blocks the UI because of file
// size. Line indexing and search run in the background and stream results.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bench;
mod document;
mod lineindex;
mod replace;
mod search;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Headless modes: `gred --bench <file>` / `gred --gen <file> <gb>`.
    if let Some(first) = args.first() {
        if first == "--bench" || first == "--gen" {
            bench::main(&args);
        }
    }

    let open_path = args.into_iter().find(|a| !a.starts_with('-'));

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1280.0, 820.0))
            .with_min_inner_size(egui::vec2(640.0, 400.0))
            .with_title("gred"),
        ..Default::default()
    };

    let res = eframe::run_native(
        "gred",
        native_options,
        Box::new(|cc| Ok(Box::new(app::Gred::new(cc, open_path)))),
    );

    if let Err(e) = &res {
        let msg = e.to_string();
        if msg.contains("opengl") || msg.contains("OpenGL") || msg.contains("NoAvailableConfig") {
            eprintln!(
                "gred: could not create an OpenGL 2.0+ context ({e}).\n\
                 This usually means a headless/RDP session with no GPU driver.\n\
                 Fix: put a software-GL `opengl32.dll` (Mesa3D llvmpipe, plus\n\
                 `libgallium_wgl.dll`) next to gred.exe, or run in a desktop session."
            );
        }
    }
    res
}
