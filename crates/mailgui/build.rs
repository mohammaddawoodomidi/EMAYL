//! Windows-only extras for the program file:
//!
//! * the program's icon, and its name/version/description as shown by Explorer,
//!   the taskbar and Task Manager (a "resource" built from `assets/`);
//! * a bigger stack: Windows gives a program's main thread only 1 MiB where Linux
//!   gives 8 MiB, and a window program draws deeply nested layouts.
//!
//! Nothing happens on other systems.

use std::path::Path;

/// The text of the Windows resource file. Kept separate from the compiling step
/// so it can be looked at on any system.
fn resource_script(icon: &Path, manifest: &Path) -> String {
    let slashes = |p: &Path| p.display().to_string().replace('\\', "/");
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| "0".to_owned());
    let (major, minor, patch) = (
        var("CARGO_PKG_VERSION_MAJOR"),
        var("CARGO_PKG_VERSION_MINOR"),
        var("CARGO_PKG_VERSION_PATCH"),
    );
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    format!(
        r#"1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "EMAYL"
            VALUE "FileDescription", "EMAYL - email client"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "emayl"
            VALUE "LegalCopyright", "Copyright (c) EMAYL contributors"
            VALUE "OriginalFilename", "emayl.exe"
            VALUE "ProductName", "EMAYL"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        icon = slashes(icon),
        manifest = slashes(manifest),
    )
}

#[cfg(windows)]
fn embed_resources() {
    use std::path::PathBuf;

    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("..")
        .join("..");
    let icon = root.join("assets").join("icon").join("emayl.ico");
    let manifest = root.join("assets").join("windows").join("emayl.manifest");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", manifest.display());

    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("emayl.rc");
    if let Err(error) = std::fs::write(&out, resource_script(&icon, &manifest)) {
        eprintln!("could not write {}: {error}", out.display());
        std::process::exit(1);
    }
    // A program without its icon and name is not what we mean to ship, so a
    // failure here stops the build instead of passing quietly.
    if let Err(error) =
        embed_resource::compile_for(&out, ["emayl"], embed_resource::NONE).manifest_required()
    {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn embed_resources() {
    // Not needed here; the function exists so the script reads the same everywhere.
    let _ = resource_script(Path::new(""), Path::new(""));
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    embed_resources();
}
