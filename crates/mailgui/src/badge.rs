//! The "you have unread mail" mark.
//!
//! * **Icon badge** ([`with_badge`]): the program's own icon with a red count bubble in its
//!   corner. It is set as the window icon, which is what the taskbar button shows on Windows
//!   and what most Linux window lists show. This is a picture drawn here, not the Windows
//!   "overlay icon" feature of the taskbar, which needs unsafe system calls that this program
//!   does not allow itself.
//! * **Dock count** ([`launcher`], Linux only): the count shown by the Ubuntu dock, KDE Plasma
//!   and other docks that follow the "Unity launcher entry" convention.
//!
//! Neither sends anything over the internet, and neither contains message text.

const RED: [u8; 3] = [0xE5, 0x39, 0x35];
const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// 5x7 pictures of the characters a badge can show; each row is 5 bits, top to bottom.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        _ => [0; 7],
    }
}

/// What the bubble says: the number, or "99+" from a hundred up.
pub fn label(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

fn blend(px: &mut [u8], rgb: [u8; 3], cover: f32) {
    let cover = cover.clamp(0.0, 1.0);
    for i in 0..3 {
        px[i] = (f32::from(px[i]) * (1.0 - cover) + f32::from(rgb[i]) * cover).round() as u8;
    }
    // The bubble is opaque where it is drawn.
    let a = f32::from(px[3]) * (1.0 - cover) + 255.0 * cover;
    px[3] = a.round() as u8;
}

/// `base` is a square RGBA picture `side` pixels wide. Returns it with a red bubble showing
/// `count` in the bottom-right corner; the same picture when `count` is 0.
pub fn with_badge(base: &[u8], side: u32, count: usize) -> Vec<u8> {
    let mut out = base.to_vec();
    let side_us = side as usize;
    if count == 0 || out.len() != side_us * side_us * 4 || side < 32 {
        return out;
    }
    let s = side as f32;
    let radius = s * 0.36;
    let (cx, cy) = (s - radius - 1.0, s - radius - 1.0);
    for y in 0..side_us {
        for x in 0..side_us {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = (y * side_us + x) * 4;
            // A white ring first, so the bubble stands out from any icon colour.
            blend(&mut out[i..i + 4], WHITE, radius + 2.0 - d + 0.5);
            blend(&mut out[i..i + 4], RED, radius - d + 0.5);
        }
    }
    // The characters, centred in the bubble.
    let text = label(count);
    let chars: Vec<char> = text.chars().collect();
    let scale = if chars.len() == 1 { 3 } else { 2 };
    let width = chars.len() * 5 * scale + (chars.len() - 1) * scale;
    let height = 7 * scale;
    let left = (cx - width as f32 / 2.0).round() as i32;
    let top = (cy - height as f32 / 2.0).round() as i32;
    for (n, ch) in chars.iter().enumerate() {
        let rows = glyph(*ch);
        let gx = left + (n * 6 * scale) as i32;
        for (ry, row) in rows.iter().enumerate() {
            for rx in 0..5 {
                if row >> (4 - rx) & 1 == 1 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = gx + (rx * scale + dx) as i32;
                            let py = top + (ry * scale + dy) as i32;
                            if (0..side as i32).contains(&px) && (0..side as i32).contains(&py) {
                                let i = (py as usize * side_us + px as usize) * 4;
                                blend(&mut out[i..i + 4], WHITE, 1.0);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Tells the dock how many unread messages there are. Does nothing where there is no dock
/// to tell, and never fails loudly.
#[cfg(target_os = "linux")]
pub fn launcher(count: usize) {
    // Talking to the session bus can wait on another program, so it never runs on the
    // window's own thread.
    let _ = std::thread::Builder::new()
        .name("dock-count".into())
        .spawn(move || {
            let _ = send_launcher_count(count);
        });
}

#[cfg(not(target_os = "linux"))]
pub fn launcher(_count: usize) {}

/// The name docks know the program by: the desktop file's name, `emayl.desktop`.
#[cfg(target_os = "linux")]
const DESKTOP_URI: &str = "application://emayl.desktop";

/// The "Unity launcher entry" convention: a signal on the user's own session bus, no files
/// and no network. Ubuntu's dock, KDE Plasma and several others listen for it.
#[cfg(target_os = "linux")]
fn send_launcher_count(count: usize) -> zbus::Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    let bus = zbus::blocking::Connection::session()?;
    let mut props: HashMap<&str, Value<'_>> = HashMap::new();
    props.insert(
        "count",
        Value::I64(i64::try_from(count).unwrap_or(i64::MAX)),
    );
    props.insert("count-visible", Value::Bool(count > 0));
    bus.emit_signal(
        None::<&str>,
        "/com/canonical/unity/launcherentry/1",
        "com.canonical.Unity.LauncherEntry",
        "Update",
        &(DESKTOP_URI, props),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn base() -> Vec<u8> {
        // A plain blue square, fully opaque.
        std::iter::repeat_n([0x20, 0x50, 0xE0, 0xFF], 64 * 64)
            .flatten()
            .collect()
    }

    fn px(img: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * 64 + x) * 4;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    #[test]
    fn no_unread_mail_means_the_plain_icon() {
        assert_eq!(with_badge(&base(), 64, 0), base());
    }

    #[test]
    fn a_bubble_is_drawn_in_the_corner_and_nowhere_else() {
        let img = with_badge(&base(), 64, 3);
        assert_ne!(img, base());
        // Top-left stays as it was; the bubble's edge area is red.
        assert_eq!(px(&img, 2, 2), [0x20, 0x50, 0xE0, 0xFF]);
        assert_eq!(px(&img, 32, 5), [0x20, 0x50, 0xE0, 0xFF]);
        let red = px(&img, 41, 55);
        assert!(red[0] > 0xC0 && red[1] < 0x60 && red[2] < 0x60, "{red:?}");
        // White digit pixels exist inside it.
        let whites = (36..62)
            .flat_map(|y| (36..62).map(move |x| (x, y)))
            .filter(|&(x, y)| px(&img, x, y)[..3] == [255, 255, 255])
            .count();
        assert!(whites > 20, "{whites}");
    }

    #[test]
    fn different_counts_look_different_and_big_counts_say_99_plus() {
        let one = with_badge(&base(), 64, 1);
        let two = with_badge(&base(), 64, 2);
        let many = with_badge(&base(), 64, 250);
        assert_ne!(one, two);
        assert_ne!(two, many);
        assert_eq!(label(7), "7");
        assert_eq!(label(99), "99");
        assert_eq!(label(100), "99+");
        assert_eq!(with_badge(&base(), 64, 100), with_badge(&base(), 64, 5000));
    }

    #[test]
    fn wrong_sized_input_is_returned_unchanged() {
        let odd = vec![7u8; 10];
        assert_eq!(with_badge(&odd, 64, 3), odd);
        assert_eq!(with_badge(&base(), 16, 3), base());
    }

    #[test]
    fn transparent_corners_become_a_solid_bubble() {
        let clear = vec![0u8; 64 * 64 * 4];
        let img = with_badge(&clear, 64, 5);
        assert_eq!(px(&img, 1, 1)[3], 0);
        assert_eq!(px(&img, 41, 55)[3], 255);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_dock_receives_the_count_over_the_session_bus() {
        use zbus::MatchRule;
        use zbus::message::Type;
        use zbus::zvariant::Value;
        // Without a session bus (a build server, say) there is no dock to tell.
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("no session bus here; dock test skipped");
            return;
        }
        let listener = zbus::blocking::Connection::session().unwrap();
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .interface("com.canonical.Unity.LauncherEntry")
            .unwrap()
            .build();
        let mut seen =
            zbus::blocking::MessageIterator::for_match_rule(rule, &listener, None).unwrap();
        send_launcher_count(7).unwrap();
        let msg = seen.next().unwrap().unwrap();
        let body = msg.body();
        let (uri, props): (String, std::collections::HashMap<String, Value<'_>>) =
            body.deserialize().unwrap();
        assert_eq!(uri, "application://emayl.desktop");
        assert_eq!(props["count"], Value::I64(7));
        assert_eq!(props["count-visible"], Value::Bool(true));
    }
}
