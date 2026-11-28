# What was tested, and what was not

## Automatic tests (run by CI on Linux and Windows, and by `cargo test --workspace`)

| Area | How it is tested |
|---|---|
| Mail protocol (IMAP, SMTP, OAuth sign-in handshakes) | Against small **simulated servers** written for the tests, including broken ones: dropped connection mid-message, silent server, rejected password, rejected token. |
| Server discovery | Simulated web servers; hostile XML; redirects; oversize replies. |
| Message display | Hostile and malformed HTML/MIME samples; spaces, links, quoting, huge input. |
| Local database | Search, threading, folder changes, corrupt input; a test checks the SQLite build options. |
| Sending and failures | Outbox states, retries, bounce recognition, reply/forward rules, address and header checks. |
| The window | A headless driver clicks through the program (software drawing, no screen needed) and saves pictures to `target/ui-snapshots/`; tests also assert on behaviour such as cancelling a sign-in or leaving a screen. |
| Hostile input | `crates/mailcore/tests/hostile_input.rs` feeds thousands of corrupted variants of real-looking messages, delivery reports, settings files, addresses, links, folder names and file names to the parsers: nothing may panic or hang and safety properties must hold. It is a fixed-seed mutation test (not a coverage-guided fuzzer). `EMAYL_FUZZ_ROUNDS=200000 cargo test -p mailcore --release --test hostile_input` runs a much longer campaign (done once by the builder: no failures). |
| Races and start-up | Two threads cancelling/starting the same outbox item; a late autosave after a discard; a second copy of the program on the same files; search text with control characters. |
| Privacy of stored and sent data | File permissions, planted links, leftovers after account removal (including the database files), database pragmas, the list of headers in a sent message. |
| Dependencies | `cargo deny check` (security notices, banned crates, licenses, sources). |
| Start-up | Linux CI starts the finished program on a virtual screen and checks it stays running for 8 seconds. |

At the time of the QA-pass delivery: 396 tests in `mailcore` (1 more is ignored: it needs a desktop password store), 9 hostile-input tests, 52 in `mailgui` (2 cover the Gmail app-password help, 1 checks the window icon, 5 the remembered window and folder, 7 on pasted app passwords and the full-address retry, 9 cover the start-up note and problem log) 36 window tests and 2 shadow-guard tests passed locally (Linux). The crash-then-switch-to-processor-drawing behaviour was also run end to end on Linux under a virtual display; it has not run on Windows. The two thread-race tests (outbox, drafts) were run 25 and 10 times in a row without a failure.

## Not tested (and why)

- **Real providers** (Gmail, Outlook, Yahoo, iCloud, …): no accounts or network access were available to the authors.
- **Real browser sign-in** (Google/Microsoft): needs the owner's registration (see `README.md`); only the surrounding logic is tested.
- **Windows behaviour**: nothing was run on Windows by the authors; the first Windows run happens in GitHub's CI.
- **Real graphics cards, system fonts, high-DPI, Wayland vs X11 differences.**
- **The real OS password store**: its test (`os_keyring_round_trip`) is marked "ignored" because it needs a desktop session; it must be run by hand on a desktop (`cargo test -p mailcore -- --ignored os_keyring`).
- **Very large mailboxes** (hundreds of thousands of messages), very slow or flaky networks over days.
- **Independent security review / coverage-guided fuzzing (cargo-fuzz, AFL)**: none yet. The mutation tests above are a cheap substitute, not an equal one.

## Optional hands-on checklist (≈ 30 minutes, for a person with a spare test mailbox)

Nobody is required to do this; it lists what automated tests cannot show. Use a throw-away account.

1. Install from the Releases page; confirm the checksum matches.
2. Add an account with an app password. The inbox fills; unread counts look right.
3. Add a second account; check the unified inbox and the per-account view.
4. Open a message with pictures and links: pictures are not loaded; the link asks before opening.
5. Write a message with an attachment to yourself; it appears in *Sent* and arrives; save the attachment.
6. Turn the network off, send a message: it waits in the *Outbox*; turn it on: it goes out by itself.
7. Send to a non-existent address at your own domain: a delivery-failure notice returns and the Outbox explains it.
8. Change the account password at the provider; the program asks you to sign in again, not to crash.
9. Quit and restart; drafts and unsent messages are still there.
10. *Settings → Copy report for support*; read it and confirm no password or address is in it.

## Packaging checks (icons and installers)

* Linux `.deb`: built from the real release program, checked with `lintian` (no findings),
  installed and removed with `apt` in a clean container; the window was opened on a virtual screen and
  its window class (`emayl`, matching the menu entry) and 64 x 64 icon were read back.
* Windows installer: `installer.nsi` compiles with NSIS 3.09 with warnings treated as errors, and the
  CI workflow repeats that on every run. **The installer and the Windows icon/version resource were never
  run on Windows**; the first real run is the release workflow, and trying the installer on a Windows
  machine is still to be done.


## The window that turned grey and then black (Windows)

Cause, found by reading iced 0.14's processor drawing code and reproduced two ways on Linux: a shadow is painted over its whole blurred area and is not clipped to the part being repainted, so each small repaint (a blinking cursor, a hover) darkens it again. A white card drawn through that code went to RGB 232 after 1 repaint, 99 after 10 and 5 after 60 (a small program drawing one shadowed card with `iced_tiny_skia` directly); with no shadow it stayed 255. The real program showed the grey card with light patches after 30 seconds under a virtual display, and stayed white after the shadows were removed. Not run on Windows. `crates/mailgui/tests/no_shadows.rs` fails if any source file asks for a shadow or uses iced's plain scroll area (whose middle-click marker has one).

## Scrolling speed (Windows, processor drawing)

Cause, measured with timers added inside iced 0.14's processor drawing code (a scratch copy, not shipped): drawing rounded boxes took 16.4 of about 26 ms per mailbox frame at 200 % scale. Two things made it slow. iced draws a box even when its fill is transparent, so every unselected mail row and every sidebar button cost a full box; they now say "no fill" instead (`theme::fill`). A box that sticks out of its scroll area, such as a tall card while scrolling, is filled through a clip mask, which is about ten times slower; `vendor/iced_tiny_skia` fills only the visible part instead (see `vendor/iced_tiny_skia/EMAYL-PATCH.md` for the numbers). After both changes the box time on the mailbox screen was 1.8 ms, and the tall add-account form drew in 15.5 ms instead of 38.5 ms. Measured on Linux, not on Windows or on a Lenovo Z50-70.

## Batch 1 of the seven-point fix list (sync stall, way home, text overflow)

- **First sync.** The first pass used to show nothing until every folder of the account had been downloaded (up to 2,000 messages each), and showed only "Syncing…". Now: the inbox is downloaded first (newest 1,000), every other folder gets its newest 200 (older mail loads on request), the lists refresh while the download runs, and the sidebar and empty list show "Downloading 340 of 1,000". Tests: `the_inbox_is_downloaded_first_and_progress_is_reported`, `a_status_watcher_sees_the_download_advance_before_it_ends` (mailcore); `the_first_download_shows_how_far_it_has_got` (picture 43).
- **Not tested:** a real Gmail account (none available here). Speed on real Gmail depends on the network and Google's own limits.
- **Way home.** Every screen other than the mail list has a "Back to mail" bar, and the sidebar always has Home / All inboxes. Test: `every_other_screen_has_a_way_back_to_the_mail` (picture 42).
- **Overflow.** Long words break at the box edge; single lines are cut at their box. Test: `long_text_stays_inside_its_boxes_in_small_windows` (pictures 40 and 41, a hostile message at the 900x560 minimum and at 1280x720).

## Batch 2 (rename, icon, dark theme, trackers)

- Rename: `old_folders_are_moved_to_the_new_names_once`, `existing_new_folders_are_never_overwritten`, `a_fresh_install_just_uses_the_new_folders` (services); `a_secret_from_the_old_name_is_moved_to_the_new_store_once`, `new_secrets_go_to_the_new_store_and_clear_stale_old_ones` (secrets). The real Windows Credential Manager / Linux keyring copy is only tested with the in-memory store here.
- Dark theme: `text_is_readable_on_every_surface_in_every_theme_and_accent` checks WCAG contrast (body text 7:1, secondary text and accent text 4.5:1, white on buttons 4.5:1) for 2 themes x 6 accents. Pictures 44-48. `look_choices_outside_the_offered_ones_are_ignored` covers the saved file.
- Trackers: five unit tests in `render/trackers.rs` plus the screen test `trackers_are_counted_and_links_are_cleaned_before_opening` (pictures 49, 50).
- Window icon test no longer expects see-through corners: the supplied picture is a full square.

## Batch 3 (unread badges, small-file permissions)

- `badge.rs`: five picture tests (plain icon at 0, bubble only in the corner, different counts differ, "99+", wrong sizes untouched). Sample pictures were looked at for 1, 7, 12 and 250.
- Dock count: `the_dock_receives_the_count_over_the_session_bus` sends the signal and reads it back on a second connection. It is skipped when there is no session bus; it was run here under `dbus-run-session` and passed. Whether a given dock (Ubuntu dock, KDE Plasma) shows the number was **not** checked: no such desktop here.
- App wiring: `unread_mail_puts_a_count_in_the_title_and_the_icon`. Setting the icon on a real window is not tested (needs a real window; the Windows taskbar was never run).
- `privfile`: new files are 0600 and a looser old file is tightened (Unix only). Checked by running the program and listing the data folder.

## Batch 4 (reading page, choosing several, small windows)

- `several_conversations_can_be_chosen_and_changed_together`: ticking, All/None, leaving the mode when the folder changes, and the real actions (`bulk_apply`, the same function the buttons call) on the test mailbox: mark read/unread, star, delete to Trash, nothing chosen. Pictures 50-52 (light, minimum size, dark).
- `small_windows_show_one_pane_at_a_time_with_a_way_back`: below 1040 interface units (window width divided by the interface size) only the list or the message is shown; Back and Escape return. Pictures 53, 54. The 1040 limit was chosen by looking at pictures at 900 and 1100 pixels, not by testing with real users.
- Reading page redesign: pictures 04 (light), 04c (dark) and 54 (small window). The "queued" server side of the bulk actions is the same code as the single-message buttons; it was exercised against the local store only, not a real mail server.

## Batch 5 (filters, blocked senders, templates, undo send, several accounts)

- mailcore: rule matching (exact address, domain and subdomains but not look-alikes, name/address text, subject, to, per account, bad input refused), first-move-wins, files that survive a restart and that are never overwritten when damaged, account removal; rules applied to *new* inbox mail during a sync against the mock server (not to the first download), "apply now", a Junk move creating the queued server command; delayed outbox items are not due before their time and can be cancelled; templates (limits, replacement by name, damaged file).
- UI tests: `blocked_senders_and_rules_can_be_added_listed_removed_and_are_applied`, `the_block_button_blocks_the_sender_and_not_yourself`, `templates_are_saved_listed_inserted_and_removed`, `undo_send_holds_the_message_and_brings_it_back`, `several_accounts_are_told_apart_and_the_list_can_show_only_unread_mail`; pictures 60-69.
- What is **not** covered: the background tasks that the buttons start are not run in the UI tests (the tests call the same functions they call); the real sending loop waking at the end of the undo time was not observed against a real mail server; "All inboxes" with two *running* accounts was not drawn (the test world starts only one account, so the account tag on rows was not seen in a picture); a real Junk folder on a real server was never used.

## Batch 6 (long lists, layout options)

- **Measured, not guessed.** `list_cost_with_many_conversations` (ignored by default; run it with `cargo test --release -p mailgui --test ui -- --ignored --nocapture list_cost`) builds and lays out the whole screen with the software renderer. Before the change, layout took 17 ms (100 rows), 95 ms (1 000), 233 ms (3 000) and 682 ms (10 000); after it, about 9-11 ms at every size. The screen is rebuilt on every message, including the 2-second tick, so the old cost was paid again and again. These numbers are from this machine's processor in a release build; they say how the cost *grows*, not how it feels on another computer. Drawing (painting pixels) and real scrolling with a mouse were not measured.
- **How the list works now.** Every row has one fixed height (78/68 px with previews, 60/50 without; roomy/compact), so the list builds only the rows near the visible part (plus 8 above and below) and blank blocks stand in for the rest. Unit tests (`view::list::window_tests`) check that the window always covers what can be seen for thousands of scroll positions and window heights, never gets large, and does not panic on absurd values (NaN, infinity, below zero). UI tests: `a_long_list_builds_only_the_rows_near_the_view` (5 000 conversations: first rows present, a far row absent; after scrolling far down, the rows there are present and the first ones are not; picture 70) and `switching_what_the_list_shows_puts_it_back_at_the_top` (folder, Unread, leaving a search).
- **Not tested:** dragging the scroll bar or using the mouse wheel in a real window (the test calls the same message the scroll area sends, but no real scrolling happened); the list's own scroll position is reset by a command whose effect on a real window was not observed. If a blank band ever shows after switching folders, scrolling once brings the rows back.
- **Layout options** (Settings → Appearance): *Where a message opens* (beside the list / on its own page) and *Width of the message list* (slim 330, normal 390, wide 480 px). A wider list needs a wider window before it sits beside a message: the one-page switch point moves from 1040 to 980 / 1040 / 1130 interface units. Tests: `the_message_can_open_on_its_own_page_even_in_a_wide_window`, `a_wider_list_needs_a_wider_window_to_sit_beside_a_message`, `list_width_pictures` (pictures 71-76; light and dark, settings page at 900x1500), and the saved-file tests in `prefs.rs`. The switch points were chosen by reasoning and by looking at pictures, not by testing with people.
- **Weak spot:** "on its own page" in a very wide window stretches the list across the whole window (picture 71): it works, but it looks sparse, and the text in each row is still cut at the same length. The reading page keeps a centred column.

## Batch 6b (Windows build fix, thread order, side bar, reading page)

- **Windows build:** CI on Windows failed with `unused import: self` in `privfile.rs` (the import was only used by Linux-only code). Fixed. This cannot be reproduced here (no Windows target installed); the fix was checked with the Linux build, and the other Linux/Windows-split code was read for the same mistake. A second Windows-only error may still appear after this one.
- **Thread order:** `the_newest_message_of_a_conversation_is_on_top` compares the on-screen positions of the newest and oldest sender. It was run against the old order too and failed there (y=511 vs 174), so it does check the change.
- **Folder names:** `pretty_name` (unit tests: `[Gmail]/`, `[Google Mail]/`, `INBOX.`/`INBOX/` removed; "Projects/Launch", "[Gmail]" alone, empty names and look-alikes such as "[Gmailer]/x" untouched). The stored name is unchanged; only the display. `gmail_folders_lose_their_wrapper_and_each_account_has_its_own_colour_and_mark` shows two running accounts for the first time (pictures 77, 78): this is the first picture with two accounts in the side bar.
- **Account colours:** by position in the account list, never equal for the first eight accounts; the colour of an account changes if an account before it is removed.
- **Reading page:** pictures 04, 04c, 49, 54. Not checked: real mail with several notices and attachments at once (only the test messages were drawn), the hover text of the privacy line and of the copy button (tooltips are not drawn by the picture test), and very long recipient lists.

## Batch 6c (library versions, Dependabot, guide)

- The `cargo clippy ... --locked` failures on GitHub came from `crates/mailcore/Cargo.toml` (updated by a merged Dependabot pull request: rusqlite 0.43, mailparse 0.19, libsqlite3-sys 0.40) disagreeing with an older `Cargo.lock` from an earlier zip. The project now uses those newer versions: fmt, clippy, all tests (including the SQLite build-settings test) and `cargo deny` pass with them; `cargo metadata --locked` passes online and offline.
- Dependabot no longer opens pull requests for Rust libraries (only for build workflows), so only one party writes `Cargo.toml`/`Cargo.lock`. Not checked: how GitHub's separate "security updates" feature behaves with this setting; the weekly `cargo deny` run is the safety net.
- `ci.yml` has a step that runs only on failure and prints what Cargo would change in `Cargo.lock`. It was seen working on a real failure (the log that led to this fix), on Linux and Windows.
- `MAINTAINING.md` was written from the failures seen in this repository; nobody has followed it start to finish yet.

## Batch 7 (opening mail, a calmer reading page)

- **What was slow.** Measured with a large newsletter (up to 250 sections, about 200 KB of page code): reading
  and drawing it takes well under 0.1 s, so drawing is not the problem. The wait is the download: the first
  time a message is opened the program must connect to the mail server and fetch it.
- **Fix.** After each successful check for mail, the newest messages in the Inbox (up to 20, none over 600 KB,
  8 MB in total) are downloaded in the background, one at a time, so that opening them is instant. A message
  you open yourself waits for at most one of these small downloads. The list refreshes its previews about
  once every two seconds instead of after every single message.
- **Reading page.** Earlier messages quoted in a reply (and the "… wrote:" line above them) are folded behind
  one *Show quoted text* button; picture notes are gone from the text; long web addresses show only the site
  (`example.com/…`); the privacy and other notes sit behind a small shield icon in the message header.
  A link whose text names one site but leads to another is never shortened or hidden.
- **Tested automatically:** which messages are chosen for background download (newest first, size and count
  limits, other accounts excluded, nothing left once downloaded), against the simulated mail server; folding
  rules; link shortening and the never-hide-a-warning rule; screens in light, dark and a small window.
- **Not tested here:** how long a real server takes (only the program's own part was measured); real clicking
  of the new buttons; Windows.
- **Not folded:** signatures, and quoted headers written out in full by Outlook ("From: … Sent: …").

## Known limits (moved here from the README)

Read this list; it is the honest state of the project.

**Never tried against real services.** Gmail, Outlook, Yahoo and others were never contacted by
the authors. Provider quirks that only a real server shows will exist. Please report them with the
*Copy report for support* text.

**Windows has not been exercised by a person.** It is built and tested by GitHub's Windows runners
only after the first push. The download-warning code (Windows "mark of the web"), the file chooser
and the look of the program on Windows are unproven.

**Only checked with a software renderer.** The window was verified with a headless drawing test, not
on real graphics hardware, real system fonts or real screens (high-DPI, dark mode, scaling).

**Security limits (it is hardened, not "absolutely secure" — no software is):**
- Connections use TLS 1.2 or newer with the operating system's certificate checks; there is no
  "ignore certificate errors" option.
- Passwords and sign-in tokens are stored only in the OS password store.
- Message HTML is rewritten to a safe subset; remote pictures, scripts and styles are **not
  loaded or shown** (pictures appear as a note). There is currently **no "load pictures" button**.
- **Mail saved on your computer is not encrypted by this program.** Turn on disk encryption
  (BitLocker on Windows, LUKS on Linux).
- **No signature checking and no decryption** (S/MIME, PGP are not supported).
- Not 100% Rust: the TLS provider (*ring*) and the bundled SQLite database contain C and
  assembly code. They are checked by `deny.toml` rules and tests, not rewritten.
- A hostile server's answers are size-capped, but it can still be slow or chatty until a timeout.
- A security review was done by the program's builder, not independently (see `SECURITY.md`).
- Sending is "at least once": after a crash at the worst moment, a message can be sent twice.
- Windows download is not code-signed.
- Nine dependency notices ("no longer maintained" and one "unsound" notice) are accepted on purpose
  and listed with reasons in `deny.toml`; none is a known attack on this program.

**Missing features:** only plain-text writing (no rich text), no signatures, no "send later", no snooze, no
automatic detection for custom domains hosted by Google/Microsoft, no search in non-spaced languages
(Chinese/Japanese), tables in HTML mail are flattened, no calendar/contacts, no pictures from the internet, no auto-update.
