// The Store owns these assets. Copy them into Tauri's embedded frontend at build
// time so browser and desktop always ship the same project component, logo and font.
fn main() {
    let source = std::path::Path::new("../service/public");
    let destination = std::path::Path::new("ui/shared");
    std::fs::create_dir_all(destination).expect("create shared UI directory");
    for (source_name, name) in [("freshness.js", "freshness.js"), ("shared/projects.js", "projects.js"), ("shared/projects.css", "projects.css"),
        ("logo.svg", "logo.svg"), ("fonts/Fraunces-Soft.woff2", "Fraunces-Soft.woff2"), ("fonts/Fraunces-OFL.txt", "Fraunces-OFL.txt")] {
        let path = source.join(source_name);
        println!("cargo:rerun-if-changed={}", path.display());
        // The copy too: ui/shared is ignored by git, so a clean checkout (git clean -x, as the
        // release build's does) removes it while the source stays unchanged, and the app would be
        // bundled without these files.
        println!("cargo:rerun-if-changed={}", destination.join(name).display());
        std::fs::copy(path, destination.join(name)).expect("copy Store project UI");
    }
    tauri_build::build()
}
