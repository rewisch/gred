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

    // `--software` (or GRED_SOFTWARE=1): force Mesa's llvmpipe software renderer.
    // Needed on machines with no usable GPU driver (headless boxes, plain RDP
    // sessions), where the default renderer path can crash. Harmless if the
    // Mesa DLLs next to the exe aren't present — the system opengl32 is used.
    if args.iter().any(|a| a == "--software") || std::env::var_os("GRED_SOFTWARE").is_some() {
        std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        std::env::set_var("MESA_GL_VERSION_OVERRIDE", "3.3");
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
                 Fix: run `gred --software` with the bundled Mesa DLLs\n\
                 (opengl32.dll + libgallium_wgl.dll) next to gred.exe, or run\n\
                 in a real desktop session."
            );
        }
    }
    res
}
