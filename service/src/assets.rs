//! The web UI (`service/public`) compiled in, so the service serves its pages with no folder
//! on the Mac. One copy of the files: the desktop build copies a few of them.

pub const FILES: [(&str, &[u8]); 10] = [
    ("apple-touch-icon.png", include_bytes!("../public/apple-touch-icon.png")),
    ("favicon.ico", include_bytes!("../public/favicon.ico")),
    ("fonts/Fraunces-OFL.txt", include_bytes!("../public/fonts/Fraunces-OFL.txt")),
    ("fonts/Fraunces-Soft.woff2", include_bytes!("../public/fonts/Fraunces-Soft.woff2")),
    ("freshness.js", include_bytes!("../public/freshness.js")),
    ("index.html", include_bytes!("../public/index.html")),
    ("login.html", include_bytes!("../public/login.html")),
    ("logo.svg", include_bytes!("../public/logo.svg")),
    ("shared/projects.css", include_bytes!("../public/shared/projects.css")),
    ("shared/projects.js", include_bytes!("../public/shared/projects.js")),
];

pub fn get(rel: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(name, _)| *name == rel).map(|(_, bytes)| *bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_file_in_public_is_embedded_and_identical() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("public");
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() { stack.push(path) } else { on_disk.push(path) }
            }
        }
        assert_eq!(on_disk.len(), super::FILES.len(), "a file was added to or removed from service/public: update assets.rs");
        for path in on_disk {
            // As a URL names it, with '/' on every OS.
            let rel = path.strip_prefix(&root).unwrap().components().map(|c| c.as_os_str().to_str().unwrap()).collect::<Vec<_>>().join("/");
            assert_eq!(super::get(&rel), Some(std::fs::read(&path).unwrap().as_slice()), "{rel}");
        }
    }
}
