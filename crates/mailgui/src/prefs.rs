//! What the program remembers between runs: how big the window was, whether it was
//! maximised, which folder was open, and the "search all folders" choice.
//!
//! One small text file (`ui-state`) in the program's data folder. Anything missing,
//! damaged or out of range is ignored, so this file can never stop the program from
//! starting. It holds no mail, no addresses and no passwords.

use std::fs;
use std::path::Path;

use iced::Size;

use crate::theme::{Accent, ThemeChoice};

pub const FILE: &str = "ui-state";

/// Smaller than this the three-column layout would be unusable.
pub const MIN_SIZE: Size = Size::new(900.0, 560.0);
/// Fits a 1366 x 768 screen with its taskbar, so a first start is never cut off.
pub const DEFAULT_SIZE: Size = Size::new(1100.0, 680.0);

const MAX_SIDE: u32 = 10_000;
const MAX_TEXT: usize = 200;

/// The interface sizes on offer, in percent.
pub const SCALES: [u16; 4] = [90, 100, 115, 130];

/// How long a message waits before it is really sent, in seconds (0 = send at once).
/// While it waits, "Undo" brings it back.
pub const SEND_DELAYS: [u8; 5] = [0, 5, 10, 20, 30];
pub const DEFAULT_SEND_DELAY: u8 = 10;

/// How wide the list of conversations is when the message is shown beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListWidth {
    Slim,
    #[default]
    Normal,
    Wide,
}

impl ListWidth {
    pub const ALL: [Self; 3] = [Self::Slim, Self::Normal, Self::Wide];

    /// Width in logical pixels.
    pub fn px(self) -> f32 {
        match self {
            Self::Slim => 330.0,
            Self::Normal => 390.0,
            Self::Wide => 480.0,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Slim => "slim",
            Self::Normal => "normal",
            Self::Wide => "wide",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Slim => "Slim",
            Self::Normal => "Normal",
            Self::Wide => "Wide",
        }
    }
}

/// How the program looks. Nothing here is private: it only holds choices of colour and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub theme: ThemeChoice,
    pub accent: Accent,
    /// Interface size in percent; always one of [`SCALES`].
    pub scale: u16,
    /// Tighter list rows.
    pub compact: bool,
    /// Show the first words of each message in the list.
    pub previews: bool,
    /// The open message is shown beside the list (when the window is wide enough); otherwise
    /// the list and the message each take the whole window, one at a time.
    pub side_by_side: bool,
    pub list_width: ListWidth,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            accent: Accent::default(),
            scale: 100,
            compact: false,
            previews: true,
            side_by_side: true,
            list_width: ListWidth::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    pub look: Look,
    /// The window's size when it was last not maximised, in logical pixels.
    pub size: Option<(u32, u32)>,
    pub maximized: bool,
    /// The account and folder that were open; `None` means "all inboxes".
    pub view: Option<(String, i64)>,
    pub search_everywhere: bool,
    /// Seconds a sent message can still be taken back; one of [`SEND_DELAYS`].
    pub send_delay: u8,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            look: Look::default(),
            size: None,
            maximized: false,
            view: None,
            search_everywhere: false,
            send_delay: DEFAULT_SEND_DELAY,
        }
    }
}

impl Prefs {
    /// The size to open the window at.
    pub fn window_size(&self) -> Size {
        match self.size {
            Some((w, h)) => Size::new(
                (w as f32).max(MIN_SIZE.width),
                (h as f32).max(MIN_SIZE.height),
            ),
            None => DEFAULT_SIZE,
        }
    }

    pub fn encode(&self) -> String {
        let mut out = String::from("version=1\n");
        if let Some((w, h)) = self.size {
            out.push_str(&format!("width={w}\nheight={h}\n"));
        }
        out.push_str(&format!("maximized={}\n", u8::from(self.maximized)));
        if let Some((account, folder)) = &self.view
            && !account.is_empty()
            && account.len() <= MAX_TEXT
            && !account.chars().any(char::is_control)
        {
            out.push_str(&format!("account={account}\nfolder={folder}\n"));
        }
        out.push_str(&format!(
            "search_everywhere={}\n",
            u8::from(self.search_everywhere)
        ));
        out.push_str(&format!("send_delay={}\n", self.send_delay));
        let l = &self.look;
        out.push_str(&format!(
            "theme={}\naccent={}\nscale={}\ncompact={}\npreviews={}\nside_by_side={}\nlist_width={}\n",
            l.theme.key(),
            l.accent.key(),
            l.scale,
            u8::from(l.compact),
            u8::from(l.previews),
            u8::from(l.side_by_side),
            l.list_width.key()
        ));
        out
    }

    /// Reads what [`encode`](Self::encode) wrote. Never fails: a line that is not
    /// understood is skipped, and a number outside a sensible range is ignored.
    pub fn decode(text: &str) -> Self {
        let (mut width, mut height) = (None, None);
        let (mut account, mut folder) = (None, None);
        let mut prefs = Self::default();
        for line in text.lines().take(32) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "width" => width = value.parse::<u32>().ok(),
                "height" => height = value.parse::<u32>().ok(),
                "maximized" => prefs.maximized = value == "1",
                "search_everywhere" => prefs.search_everywhere = value == "1",
                "theme" => {
                    if let Some(t) = ThemeChoice::from_key(value) {
                        prefs.look.theme = t;
                    }
                }
                "accent" => {
                    if let Some(a) = Accent::from_key(value) {
                        prefs.look.accent = a;
                    }
                }
                "scale" => {
                    if let Some(s) = value.parse::<u16>().ok().filter(|s| SCALES.contains(s)) {
                        prefs.look.scale = s;
                    }
                }
                "send_delay" => {
                    if let Some(s) = value.parse::<u8>().ok().filter(|s| SEND_DELAYS.contains(s)) {
                        prefs.send_delay = s;
                    }
                }
                "compact" => prefs.look.compact = value == "1",
                "previews" => prefs.look.previews = value != "0",
                "side_by_side" => prefs.look.side_by_side = value != "0",
                "list_width" => {
                    if let Some(w) = ListWidth::from_key(value) {
                        prefs.look.list_width = w;
                    }
                }
                "account" if !value.is_empty() && value.len() <= MAX_TEXT => {
                    account = Some(value.to_owned());
                }
                "folder" => folder = value.parse::<i64>().ok(),
                _ => {}
            }
        }
        if let (Some(w), Some(h)) = (width, height)
            && (400..=MAX_SIDE).contains(&w)
            && (300..=MAX_SIDE).contains(&h)
        {
            prefs.size = Some((w, h));
        }
        if let (Some(a), Some(f)) = (account, folder)
            && f > 0
        {
            prefs.view = Some((a, f));
        }
        prefs
    }

    pub fn load(dir: &Path) -> Self {
        match fs::read(dir.join(FILE)) {
            // Only the first bytes matter; a huge file is not ours.
            Ok(bytes) => Self::decode(&String::from_utf8_lossy(&bytes[..bytes.len().min(4096)])),
            Err(_) => Self::default(),
        }
    }

    /// Writes the file in one piece (a half-written file is never seen). Errors are
    /// ignored on purpose: forgetting the window size is not worth a message.
    pub fn save(&self, dir: &Path) {
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let temporary = dir.join(format!("{FILE}.tmp"));
        if crate::privfile::write(&temporary, self.encode().as_bytes()).is_ok() {
            let _ = fs::rename(&temporary, dir.join(FILE));
        }
    }
}

/// Shrinks a window that would not fit on the screen. The screen size is only
/// known once the window exists, so this runs just after it opens.
pub fn fit_to_screen(window: Size, screen: Size) -> Option<Size> {
    if screen.width < 100.0 || screen.height < 100.0 {
        return None;
    }
    // Room for the title bar and the taskbar.
    let width = window.width.min(screen.width * 0.94);
    let height = window.height.min(screen.height * 0.9);
    let fitted = Size::new(
        // Never below the minimum, unless the screen itself is smaller than that.
        width.max(MIN_SIZE.width.min(screen.width * 0.97)),
        height.max(MIN_SIZE.height.min(screen.height * 0.95)),
    );
    ((fitted.width - window.width).abs() > 1.0 || (fitted.height - window.height).abs() > 1.0)
        .then_some(fitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_was_saved_comes_back() {
        let p = Prefs {
            size: Some((1234, 777)),
            maximized: true,
            view: Some(("acct-1".into(), 42)),
            search_everywhere: true,
            send_delay: 20,
            look: Look {
                theme: ThemeChoice::Dark,
                accent: Accent::Teal,
                scale: 115,
                compact: true,
                previews: false,
                side_by_side: false,
                list_width: ListWidth::Wide,
            },
        };
        assert_eq!(Prefs::decode(&p.encode()), p);
        let q = Prefs::default();
        assert_eq!(Prefs::decode(&q.encode()), q);
    }

    #[test]
    fn look_choices_outside_the_offered_ones_are_ignored() {
        let p = Prefs::decode(
            "theme=neon\naccent=Plaid\nscale=999\nsend_delay=3600\nlist_width=huge\n",
        );
        assert_eq!(p.look, Look::default());
        assert_eq!(p.send_delay, DEFAULT_SEND_DELAY);
        let p = Prefs::decode("scale=130\ntheme=dark\n");
        assert_eq!(p.look.scale, 130);
        assert_eq!(p.look.theme, ThemeChoice::Dark);
        // Files from before these choices existed still load, with the defaults.
        let old = Prefs::decode("version=1\nwidth=1000\nheight=700\nmaximized=0\n");
        assert_eq!(old.look, Look::default());
    }

    #[test]
    fn nonsense_is_ignored_and_never_fails() {
        assert_eq!(Prefs::decode(""), Prefs::default());
        assert_eq!(
            Prefs::decode("\u{0}\u{1}garbage\nwidth=abc\n=\n"),
            Prefs::default()
        );
        // Out-of-range sizes and folder numbers are dropped, not trusted.
        let p = Prefs::decode("width=99999\nheight=5\naccount=a\nfolder=-3\nmaximized=1\n");
        assert_eq!(p.size, None);
        assert_eq!(p.view, None);
        assert!(p.maximized);
        // A huge file is harmless.
        let _ = Prefs::decode(&"x=1\n".repeat(200_000));
    }

    #[test]
    fn a_remembered_size_is_never_smaller_than_the_minimum() {
        let p = Prefs {
            size: Some((500, 400)),
            ..Prefs::default()
        };
        assert_eq!(p.window_size(), MIN_SIZE);
        assert_eq!(Prefs::default().window_size(), DEFAULT_SIZE);
    }

    #[test]
    fn the_file_round_trips_on_disk_and_a_missing_one_is_fine() {
        let dir = std::env::temp_dir().join(format!("emayl-prefs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(Prefs::load(&dir), Prefs::default());
        let p = Prefs {
            size: Some((1000, 700)),
            view: Some(("x".into(), 7)),
            ..Prefs::default()
        };
        p.save(&dir);
        assert_eq!(Prefs::load(&dir), p);
        fs::write(dir.join(FILE), [0xff, 0xfe, 0x00]).unwrap_or_default();
        assert_eq!(Prefs::load(&dir), Prefs::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_bigger_than_the_screen_is_shrunk_and_a_fitting_one_is_left_alone() {
        // 1366 x 768 laptop at 100 %.
        let screen = Size::new(1366.0, 768.0);
        assert_eq!(fit_to_screen(DEFAULT_SIZE, screen), None);
        let big = fit_to_screen(Size::new(1600.0, 900.0), screen).unwrap_or(Size::ZERO);
        assert!(
            big.width <= 1366.0 * 0.94 + 0.5 && big.height <= 768.0 * 0.9 + 0.5,
            "{big:?}"
        );
        // 1366 x 768 at 125 % scaling is 1092 x 614 logical pixels.
        let scaled = fit_to_screen(DEFAULT_SIZE, Size::new(1092.0, 614.0)).unwrap_or(Size::ZERO);
        assert!(
            scaled.width <= 1092.0 && scaled.height <= 614.0,
            "{scaled:?}"
        );
        assert!(
            scaled.width >= MIN_SIZE.width && scaled.height >= MIN_SIZE.height,
            "{scaled:?}"
        );
        // No usable screen size: leave the window alone.
        assert_eq!(fit_to_screen(DEFAULT_SIZE, Size::new(0.0, 0.0)), None);
    }
}
