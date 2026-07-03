use std::path::Path;

#[test]
fn test_no_module_source_conflicts() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    check_dir_for_conflicts(&src);
}

fn check_dir_for_conflicts(dir: &Path) {
    if !dir.is_dir() {
        return;
    }

    let entries = std::fs::read_dir(dir).expect("read_dir");
    let mut names = std::collections::HashSet::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();

        if path.is_dir() {
            if names.contains(&name) {
                // If we already saw name.rs, this is a conflict
                panic!(
                    "Module conflict detected: {} and {}.rs both exist",
                    path.display(),
                    name
                );
            }
            names.insert(name.clone());
            check_dir_for_conflicts(&path);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            if name == "mod" || name == "lib" || name == "main" {
                continue;
            }
            if names.contains(&name) {
                // If we already saw a directory with this name, this is a conflict
                panic!(
                    "Module conflict detected: {} and {}.rs both exist",
                    dir.join(&name).display(),
                    name
                );
            }
            names.insert(name);
        }
    }
}
