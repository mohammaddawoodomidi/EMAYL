//! Guards the fix for the window that slowly turned grey and then black on Windows.
//!
//! On Windows the program draws with the processor. iced 0.14's processor drawing paints a
//! shadow over its whole blurred area and does not clip it to the part being repainted, so each
//! small repaint (a blinking cursor, a hover) darkens the shadow's area again. After a few
//! dozen repaints a white card is nearly black. The program therefore must not use shadows.
//! The look comes from thin borders instead.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_source_file_asks_for_a_shadow() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(files.len() > 10, "found only {} source files", files.len());
    let banned = ["Shadow {", ".shadow(", "shadow:", "blur_radius"];
    let mut found = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap_or_default();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if banned.iter().any(|word| code.contains(word)) {
                found.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "shadows are not allowed:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_scroll_style_has_no_shadow_and_plain_scroll_areas_are_not_used() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    for file in files {
        if file.ends_with("theme.rs") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap_or_default();
        for line in text.lines() {
            let code = line.split("//").next().unwrap_or("");
            assert!(
                !code.contains("widget::scrollable(") && !code.contains(" scrollable("),
                "{}: use theme::scrolling instead of scrollable (its default style has a shadow)",
                file.display()
            );
        }
    }
}
