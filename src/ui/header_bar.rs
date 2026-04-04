//! Header bar — playback controls, now-playing widget, progress, volume, menu.
//!
//! Follows modern GNOME app patterns (Ptyxis-style): a primary `MenuButton`
//! with a `gio::Menu` popover on the right, rather than a legacy hamburger.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::Align;

/// Repeat button cycles through these modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

/// Interactive widgets exposed for the integration bridge to drive.
#[allow(dead_code)]
pub struct HeaderBarWidgets {
    pub header: adw::HeaderBar,
    pub play_button: gtk::Button,
    pub prev_button: gtk::Button,
    pub next_button: gtk::Button,
    pub repeat_button: gtk::ToggleButton,
    pub repeat_mode: Rc<Cell<RepeatMode>>,
    pub shuffle_button: gtk::ToggleButton,
    pub album_art: gtk::Image,
    pub title_label: gtk::Label,
    pub artist_label: gtk::Label,
    pub progress: gtk::Scale,
    pub progress_adj: gtk::Adjustment,
    pub position_label: gtk::Label,
    pub duration_label: gtk::Label,
    pub volume_scale: gtk::Scale,
    pub volume_adj: gtk::Adjustment,
}

/// Build the full header bar and return all interactive widgets.
pub fn build_header_bar() -> HeaderBarWidgets {
    // ── Left: Playback Controls ──────────────────────────────────────
    let btn_prev = gtk::Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .tooltip_text("Previous")
        .build();

    let btn_play = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Play")
        .css_classes(["suggested-action", "circular"])
        .build();

    let btn_next = gtk::Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .tooltip_text("Next")
        .build();

    let repeat_mode: Rc<Cell<RepeatMode>> = Rc::new(Cell::new(RepeatMode::Off));
    let btn_repeat = gtk::ToggleButton::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .tooltip_text("Repeat: Off")
        .build();

    // Cycle Off → All → One on each click.
    // We use a ToggleButton for the highlight but manage `active` manually.
    {
        let mode = repeat_mode.clone();
        let btn = btn_repeat.clone();
        btn_repeat.connect_clicked(move |_| {
            // The toggle already flipped `active` before this handler runs.
            // Determine the next mode from the PREVIOUS mode, not from
            // the toggle state, so we cycle correctly through 3 states.
            let next = match mode.get() {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            };
            mode.set(next);
            let (icon, tooltip, active) = match next {
                RepeatMode::Off => ("media-playlist-repeat-symbolic", "Repeat: Off", false),
                RepeatMode::All => ("media-playlist-repeat-symbolic", "Repeat: All", true),
                RepeatMode::One => ("media-playlist-repeat-song-symbolic", "Repeat: One", true),
            };
            btn.set_icon_name(icon);
            btn.set_tooltip_text(Some(tooltip));
            btn.set_active(active);
        });
    }

    let btn_shuffle = gtk::ToggleButton::builder()
        .icon_name("media-playlist-shuffle-symbolic")
        .tooltip_text("Shuffle")
        .build();

    let playback_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .valign(Align::Center)
        .build();
    playback_box.append(&btn_prev);
    playback_box.append(&btn_play);
    playback_box.append(&btn_next);
    playback_box.append(&btn_repeat);
    playback_box.append(&btn_shuffle);

    // ── Center: Now Playing ──────────────────────────────────────────
    let album_art = gtk::Image::builder()
        .icon_name("audio-x-generic-symbolic")
        .pixel_size(36)
        .css_classes(["album-art-placeholder"])
        .build();

    let title_label = gtk::Label::builder()
        .label("Not Playing")
        .css_classes(["heading"])
        .halign(Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(30)
        .build();

    let artist_label = gtk::Label::builder()
        .label("")
        .css_classes(["dim-label"])
        .halign(Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(30)
        .build();

    let text_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(Align::Center)
        .spacing(0)
        .build();
    text_box.append(&title_label);
    text_box.append(&artist_label);

    let now_playing = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .valign(Align::Center)
        .build();
    now_playing.append(&album_art);
    now_playing.append(&text_box);

    // ── Right: Progress + Volume + Menu ──────────────────────────────
    let position_label = gtk::Label::builder()
        .label("0:00")
        .css_classes(["dim-label", "caption", "numeric"])
        .width_chars(5)
        .halign(Align::End)
        .valign(Align::Center)
        .build();

    let progress_adj = gtk::Adjustment::new(0.0, 0.0, 1.0, 1000.0, 10000.0, 0.0);
    let progress = gtk::Scale::builder()
        .orientation(gtk::Orientation::Horizontal)
        .draw_value(false)
        .hexpand(false)
        .width_request(200)
        .valign(Align::Center)
        .adjustment(&progress_adj)
        .build();
    progress.add_css_class("progress-scrubber");

    let duration_label = gtk::Label::builder()
        .label("0:00")
        .css_classes(["dim-label", "caption", "numeric"])
        .width_chars(5)
        .halign(Align::Start)
        .valign(Align::Center)
        .build();

    let progress_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .valign(Align::Center)
        .build();
    progress_box.append(&position_label);
    progress_box.append(&progress);
    progress_box.append(&duration_label);

    // Volume: use a Scale + speaker icon since VolumeButton is deprecated in GTK 4.10+
    let volume_icon = gtk::Image::builder()
        .icon_name("audio-volume-high-symbolic")
        .build();
    let volume_adj = gtk::Adjustment::new(1.0, 0.0, 1.0, 0.05, 0.1, 0.0);
    let volume_scale = gtk::Scale::builder()
        .orientation(gtk::Orientation::Horizontal)
        .draw_value(false)
        .width_request(80)
        .valign(Align::Center)
        .adjustment(&volume_adj)
        .build();

    let volume_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .valign(Align::Center)
        .build();
    volume_box.append(&volume_icon);
    volume_box.append(&volume_scale);

    // Modern GNOME primary menu (Ptyxis-style)
    let menu = gtk::gio::Menu::new();
    let section1 = gtk::gio::Menu::new();
    section1.append(Some("_Preferences"), Some("app.preferences"));
    section1.append(Some("_About Tributary"), Some("app.about"));
    menu.append_section(None, &section1);
    let section2 = gtk::gio::Menu::new();
    section2.append(Some("_Quit"), Some("app.quit"));
    menu.append_section(None, &section2);

    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .primary(true)
        .valign(Align::Center)
        .build();

    let right_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .valign(Align::Center)
        .build();
    right_box.append(&progress_box);
    right_box.append(&volume_box);

    // ── Assemble ─────────────────────────────────────────────────────
    let header = adw::HeaderBar::builder().title_widget(&now_playing).build();

    header.pack_start(&playback_box);
    header.pack_end(&menu_btn);
    header.pack_end(&right_box);

    HeaderBarWidgets {
        header,
        play_button: btn_play,
        prev_button: btn_prev,
        next_button: btn_next,
        repeat_button: btn_repeat,
        repeat_mode,
        shuffle_button: btn_shuffle,
        album_art,
        title_label,
        artist_label,
        progress,
        progress_adj,
        position_label,
        duration_label,
        volume_scale,
        volume_adj,
    }
}
