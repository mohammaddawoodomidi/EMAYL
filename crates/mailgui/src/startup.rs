//! What happens around the window: before it opens, and after it closes or breaks.
//!
//! Two jobs:
//!
//! 1. **Never fail silently.** The Windows program has no console, so a crash used
//!    to look like "a window flashes and vanishes". Now every error and every
//!    panic is written to `problems.log` in the program's data folder and, on
//!    Windows, shown in a message box that says where that file is.
//! 2. **Heal a broken graphics driver by itself.** The window is normally drawn by
//!    the graphics card. On some computers (old drivers, virtual machines) that
//!    crashes the whole program, which nothing inside the program can catch. So a
//!    small note (`startup-state`) is kept: "a start is in progress". When the
//!    program has run for a few seconds, or closes normally, the note is cleared.
//!    If the next start finds the note still there, the last start died, and the
//!    program starts again drawing with the processor instead (slower, but it
//!    works everywhere).
//!
//! Nothing here touches mail, passwords or accounts, and nothing is sent anywhere.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::services::Paths;

const STATE_FILE: &str = "startup-state";
/// Bumped when the meaning of the note changes; older notes are then ignored.
const NOTE_VERSION: u32 = 2;
const LOG_FILE: &str = "problems.log";
/// The log is emptied when it grows past this, so it can never fill the disk.
const LOG_LIMIT: u64 = 256 * 1024;
/// A start counts as good after the window has been alive this long.
const SETTLE: Duration = Duration::from_secs(4);
/// The setting the drawing library reads to choose its renderer.
const BACKEND_VARIABLE: &str = "ICED_BACKEND";
const SOFTWARE_BACKEND: &str = "tiny-skia";
/// The Windows program contains no graphics-card drawing at all (see `Cargo.toml`), so
/// there is nothing to switch.
const GPU_DRAWING_BUILT_IN: bool = cfg!(not(windows));

/// What the note on disk says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct State {
    /// Drawing with the processor instead of the graphics card.
    pub software: bool,
    /// A start began and was never marked as good.
    pub running: bool,
}

impl State {
    /// What a computer starts with before anything has been learned about it.
    ///
    /// Windows laptops with two graphics chips (Intel plus NVIDIA, "Optimus"), and old
    /// Intel chips, often make a window drawn by the graphics card flash between white
    /// and black. Drawing with the processor looks the same and has no such problem,
    /// and a mail window does not need the graphics card, so Windows starts that way.
    pub fn fresh() -> Self {
        Self {
            software: cfg!(windows),
            running: false,
        }
    }
}

/// What to do for this start, given the note left by the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    pub software: bool,
    /// The last start died even though it was already drawing with the processor.
    pub tell_user: bool,
    /// The last start died on the graphics card, so this one switches over.
    pub switched: bool,
}

pub(crate) fn plan(previous: State) -> Plan {
    match (previous.running, previous.software) {
        (true, false) => Plan {
            software: true,
            tell_user: false,
            switched: true,
        },
        (true, true) => Plan {
            software: true,
            tell_user: true,
            switched: false,
        },
        (false, software) => Plan {
            software,
            tell_user: false,
            switched: false,
        },
    }
}

pub(crate) fn encode(state: State) -> String {
    format!(
        "version={NOTE_VERSION}\nsoftware={}\nrunning={}\n",
        u8::from(state.software),
        u8::from(state.running)
    )
}

/// Anything unreadable, and any note written by an older version of the program
/// (which chose the graphics card), counts as a first start. Never an error.
pub(crate) fn decode(text: &str) -> State {
    let mut state = State::fresh();
    let mut current = false;
    for line in text.lines().take(8) {
        match line.trim() {
            line if line == format!("version={NOTE_VERSION}") => current = true,
            "software=1" => state.software = true,
            "software=0" => state.software = false,
            "running=1" => state.running = true,
            _ => {}
        }
    }
    if current { state } else { State::fresh() }
}

/// One run of the program.
struct Session {
    dir: Option<PathBuf>,
    software: bool,
    started: Instant,
    settled: AtomicBool,
}

static SESSION: OnceLock<Session> = OnceLock::new();

/// Called from the window's regular heartbeat. Once the window has been alive for
/// a few seconds the start is marked as good. Cheap after the first time.
pub fn still_alive() {
    let Some(session) = SESSION.get() else {
        return;
    };
    if session.settled.load(Ordering::Relaxed) || session.started.elapsed() < SETTLE {
        return;
    }
    session.settled.store(true, Ordering::Relaxed);
    mark_finished(session);
}

fn mark_finished(session: &Session) {
    if let Some(dir) = &session.dir {
        write_state(
            dir,
            State {
                software: session.software,
                running: false,
            },
        );
    }
}

/// Which kind of drawing this run uses, for the problem report.
pub fn drawing() -> &'static str {
    if !GPU_DRAWING_BUILT_IN {
        "processor (the only kind in this build)"
    } else if SESSION.get().is_some_and(|s| s.software) {
        "processor"
    } else {
        "graphics card"
    }
}

/// The whole program: called by `main`.
pub fn main() -> ExitCode {
    let dir = Paths::platform_default().map(|p| p.data);
    install_panic_hook(dir.clone());
    let previous = dir.as_deref().map_or_else(State::fresh, read_state);
    let mut plan = plan(previous);
    // For people who want the graphics card where it exists: EMAYL_RENDERER=gpu.
    if GPU_DRAWING_BUILT_IN
        && std::env::var("EMAYL_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("gpu"))
    {
        plan.software = false;
    }
    let software_requested = std::env::var(BACKEND_VARIABLE)
        .map(|v| v.split(',').any(|b| b == SOFTWARE_BACKEND))
        .unwrap_or(false);

    // The drawing library reads its choice from the environment at start-up, and
    // a running program may not change its own environment, so the program starts
    // a second copy of itself with the right setting and ends this one.
    if GPU_DRAWING_BUILT_IN
        && plan.software
        && std::env::var_os(BACKEND_VARIABLE).is_none()
        && relaunch_in_software_mode()
    {
        return ExitCode::SUCCESS;
    }

    if plan.switched {
        record(
            dir.as_deref(),
            "The last start ended unexpectedly while drawing with the graphics card. \
             Switching to drawing with the processor.",
        );
    }
    if plan.tell_user {
        let message = format!(
            "EMAYL did not start properly last time.\n\n\
             A note about what happened is kept here:\n{}\n\n\
             If it keeps happening, send that file to whoever is helping you.",
            log_path_text(dir.as_deref())
        );
        show_message("EMAYL", &message);
    }

    let software = plan.software || software_requested;
    let session = Session {
        dir: dir.clone(),
        software,
        started: Instant::now(),
        settled: AtomicBool::new(false),
    };
    if let Some(dir) = &session.dir {
        write_state(
            dir,
            State {
                software,
                running: true,
            },
        );
    }
    // Setting it can only fail if it was already set, which cannot happen here.
    let _ = SESSION.set(session);

    match crate::run() {
        Ok(()) => {
            if let Some(session) = SESSION.get() {
                session.settled.store(true, Ordering::Relaxed);
                mark_finished(session);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            let text = format!("The window could not be opened: {error}");
            record(dir.as_deref(), &text);
            show_message(
                "EMAYL could not start",
                &format!(
                    "{text}\n\nA note about what happened is kept here:\n{}",
                    log_path_text(dir.as_deref())
                ),
            );
            ExitCode::FAILURE
        }
    }
}

fn relaunch_in_software_mode() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(BACKEND_VARIABLE, SOFTWARE_BACKEND)
        .spawn()
        .is_ok()
}

// ----- the note on disk ---------------------------------------------------------

fn read_state(dir: &Path) -> State {
    // A huge or odd file is not ours; only the first bytes matter.
    match fs::read(dir.join(STATE_FILE)) {
        Ok(bytes) => decode(&String::from_utf8_lossy(&bytes[..bytes.len().min(256)])),
        Err(_) => State::fresh(),
    }
}

fn write_state(dir: &Path, state: State) {
    if make_dir(dir).is_ok() {
        let _ = crate::privfile::write(&dir.join(STATE_FILE), encode(state).as_bytes());
    }
}

fn make_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

// ----- the problem log ----------------------------------------------------------

fn log_path_text(dir: Option<&Path>) -> String {
    dir.map_or_else(
        || "(this computer has no place for program files)".to_owned(),
        |d| d.join(LOG_FILE).display().to_string(),
    )
}

/// Adds one entry. Entries contain only the program's version, the kind of
/// computer and the error text; never mail, addresses or passwords.
pub(crate) fn record(dir: Option<&Path>, text: &str) {
    let Some(dir) = dir else {
        return;
    };
    if make_dir(dir).is_err() {
        return;
    }
    let path = dir.join(LOG_FILE);
    if fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = fs::remove_file(&path);
    }
    let when = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let Ok(mut file) = crate::privfile::append(&path) else {
        return;
    };
    let _ = writeln!(
        file,
        "[unix time {when}] EMAYL {} on {} {}: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        text.replace(['\r', '\n'], " ")
    );
}

fn install_panic_hook(dir: Option<PathBuf>) {
    std::panic::set_hook(Box::new(move |info| {
        let text = describe_panic(info);
        record(dir.as_deref(), &text);
        eprintln!("{text}");
        show_message(
            "EMAYL stopped unexpectedly",
            &format!(
                "Something went wrong inside the program and it has to close.\n\n\
                 A note about what happened is kept here:\n{}",
                log_path_text(dir.as_deref())
            ),
        );
    }));
}

fn describe_panic(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_owned());
    let place = info
        .location()
        .map_or_else(String::new, |l| format!(" at {}:{}", l.file(), l.line()));
    format!("The program stopped: {message}{place}")
}

/// A message box. Only Windows needs it (there is no console to read); on other
/// systems the text goes to the terminal the program was started from.
fn show_message(title: &str, text: &str) {
    if !cfg!(windows) {
        return;
    }
    let _ = rfd::MessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(software: bool, running: bool) -> State {
        State { software, running }
    }

    #[test]
    fn a_clean_last_start_changes_nothing() {
        let p = plan(state(false, false));
        assert!(!p.software && !p.tell_user && !p.switched);
        let p = plan(state(true, false));
        assert!(p.software && !p.tell_user && !p.switched);
    }

    #[test]
    fn a_start_that_died_on_the_graphics_card_switches_to_the_processor() {
        let p = plan(state(false, true));
        assert!(p.software && p.switched && !p.tell_user);
    }

    #[test]
    fn a_start_that_died_even_on_the_processor_tells_the_user() {
        let p = plan(state(true, true));
        assert!(p.software && p.tell_user && !p.switched);
    }

    #[test]
    fn the_note_survives_a_round_trip_and_ignores_garbage() {
        for s in [
            state(false, false),
            state(true, false),
            state(false, true),
            state(true, true),
        ] {
            assert_eq!(decode(&encode(s)), s);
        }
        assert_eq!(decode(""), State::fresh());
        assert_eq!(decode("\u{0}\u{1}\u{2} software=2 running"), State::fresh());
        assert_eq!(decode(&"x\n".repeat(100_000)), State::fresh());
    }

    #[test]
    fn a_missing_or_damaged_note_file_means_nothing_special() {
        let dir = std::env::temp_dir().join(format!("emayl-startup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(read_state(&dir), State::fresh());
        write_state(&dir, state(true, true));
        assert_eq!(read_state(&dir), state(true, true));
        fs::write(dir.join(STATE_FILE), [0xff, 0xfe, 0x00, 0x80]).unwrap_or_default();
        assert_eq!(read_state(&dir), State::fresh());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_problem_log_is_one_line_per_entry_and_cannot_grow_without_end() {
        let dir = std::env::temp_dir().join(format!("emayl-problems-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        record(Some(&dir), "first\nsecond\r\nthird");
        record(Some(&dir), "another");
        let text = fs::read_to_string(dir.join(LOG_FILE)).unwrap_or_default();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("first second  third"), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")));

        let big = "x".repeat(1000);
        for _ in 0..400 {
            record(Some(&dir), &big);
        }
        let size = fs::metadata(dir.join(LOG_FILE)).map_or(u64::MAX, |m| m.len());
        assert!(size <= LOG_LIMIT + 2000, "log grew to {size}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_note_from_an_older_version_is_treated_as_a_first_start() {
        // The older program wrote no version line and chose the graphics card.
        let old = "software=0\nrunning=0\n";
        assert_eq!(decode(old), State::fresh());
        assert_eq!(decode("software=1\nrunning=1\n"), State::fresh());
        // The same words with the current version line are believed.
        let now = format!("version={NOTE_VERSION}\nsoftware=0\nrunning=0\n");
        assert_eq!(decode(&now), state(false, false));
    }

    #[test]
    fn windows_starts_with_the_processor_and_other_systems_with_the_graphics_card() {
        let first = State::fresh();
        assert_eq!(first.software, cfg!(windows));
        assert!(!first.running);
        assert_eq!(plan(first).software, cfg!(windows));
    }

    #[test]
    fn recording_without_a_folder_does_nothing() {
        record(None, "nowhere to write");
    }
}
