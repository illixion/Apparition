// The Store owns these assets. Copy them into Tauri's embedded frontend at build
// time so browser and desktop always ship the same project component.
fn main() {
    let source = std::path::Path::new("../store/public");
    let destination = std::path::Path::new("ui/shared");
    std::fs::create_dir_all(destination).expect("create shared UI directory");
    for (source_name, name) in [("freshness.js", "freshness.js"), ("shared/projects.js", "projects.js"), ("shared/projects.css", "projects.css")] {
        let path = source.join(source_name);
        println!("cargo:rerun-if-changed={}", path.display());
        std::fs::copy(path, destination.join(name)).expect("copy Store project UI");
    }
    tauri_build::build()
}
