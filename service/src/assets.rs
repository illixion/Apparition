//! The web UI (`store/public`) compiled in, so the service serves its pages with no checkout of
//! `store/` on the Mac. One copy of the files: the Node store reads the same directory.

pub const FILES: [(&str, &[u8]); 9] = [
    ("apple-touch-icon.png", include_bytes!("../../store/public/apple-touch-icon.png")),
    ("favicon.ico", include_bytes!("../../store/public/favicon.ico")),
    ("favicon.svg", include_bytes!("../../store/public/favicon.svg")),
    ("freshness.js", include_bytes!("../../store/public/freshness.js")),
    ("index.html", include_bytes!("../../store/public/index.html")),
    ("login.html", include_bytes!("../../store/public/login.html")),
    ("mask-icon.svg", include_bytes!("../../store/public/mask-icon.svg")),
    ("shared/projects.css", include_bytes!("../../store/public/shared/projects.css")),
    ("shared/projects.js", include_bytes!("../../store/public/shared/projects.js")),
];

pub fn get(rel: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(name, _)| *name == rel).map(|(_, bytes)| *bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_file_in_store_public_is_embedded_and_identical() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../store/public");
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() { stack.push(path) } else { on_disk.push(path) }
            }
        }
        assert_eq!(on_disk.len(), super::FILES.len(), "a file was added to or removed from store/public: update assets.rs");
        for path in on_disk {
            // As a URL names it, with '/' on every OS.
            let rel = path.strip_prefix(&root).unwrap().components().map(|c| c.as_os_str().to_str().unwrap()).collect::<Vec<_>>().join("/");
            assert_eq!(super::get(&rel), Some(std::fs::read(&path).unwrap().as_slice()), "{rel}");
        }
    }
}
