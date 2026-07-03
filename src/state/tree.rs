//! Directory tree display
//!
//! Prefers `tree` command if available, falls back to walkdir.

use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub fn print_tree(root: &Path) -> Result<()> {
    use colored::Colorize;

    println!("{}", "Tree".cyan().bold());

    // Try `tree` command first (fast, respects .gitignore)
    if let Ok(output) = Command::new("tree")
        .args(["-L", "3", "--gitignore", "-C"])
        .current_dir(root)
        .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            println!("  {}", line);
        }
        println!();
        return Ok(());
    }

    // Fallback: walkdir
    print_tree_walkdir(root)?;
    println!();

    Ok(())
}

fn print_tree_walkdir(root: &Path) -> Result<()> {
    use walkdir::WalkDir;

    let walker = WalkDir::new(root)
        .max_depth(3)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            // Skip hidden dirs, node_modules, target, .git
            !name.starts_with('.')
                && name != "node_modules"
                && name != "target"
                && name != "__pycache__"
        });

    for entry in walker {
        let entry = entry?;
        let depth = entry.depth();
        if depth == 0 {
            continue;
        }
        let indent = "  ".repeat(depth);
        let name = entry.file_name().to_str().unwrap_or("?");
        if entry.file_type().is_dir() {
            println!("  {}{}/", indent, name);
        } else {
            println!("  {}{}", indent, name);
        }
    }

    Ok(())
}
