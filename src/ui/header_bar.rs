//! Header bar — playback controls, now-playing widget, progress, volume, menu.
//!
//! Follows modern GNOME app patterns (Ptyxis-style): a primary `MenuButton`
//! with a `gio::Menu` popover on the right, rather than a legacy hamburger.

use adw::prelude::*;
use gtk::Align;

/// Build the full header bar and return the `adw::HeaderBar`.
pub fn build_header_bar() -> adw::HeaderBar {
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

    let btn_repeat = gtk::ToggleButton::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .tooltip_text("Repeat")
        .build();

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
    let progress = gtk::Scale::builder()
        .orientation(gtk::Orientation::Horizontal)
        .draw_value(false)
        .hexpand(false)
        .width_request(200)
        .valign(Align::Center)
        .adjustment(&gtk::Adjustment::new(0.0, 0.0, 100.0, 1.0, 10.0, 0.0))
        .build();
    progress.add_css_class("progress-scrubber");

    // Volume: use a Scale + speaker icon since VolumeButton is deprecated in GTK 4.10+
    let volume_icon = gtk::Image::builder()
        .icon_name("audio-volume-high-symbolic")
        .build();
    let volume_scale = gtk::Scale::builder()
        .orientation(gtk::Orientation::Horizontal)
        .draw_value(false)
        .width_request(80)
        .valign(Align::Center)
        .adjustment(&gtk::Adjustment::new(0.7, 0.0, 1.0, 0.05, 0.1, 0.0))
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
    menu.append(Some("_Preferences"), Some("app.preferences"));
    menu.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
    menu.append(Some("_About Tributary"), Some("app.about"));

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
    right_box.append(&progress);
    right_box.append(&volume_box);

    // ── Assemble ─────────────────────────────────────────────────────
    let header = adw::HeaderBar::builder().title_widget(&now_playing).build();

    header.pack_start(&playback_box);
    header.pack_end(&menu_btn);
    header.pack_end(&right_box);

    header
}
