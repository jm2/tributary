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
    pub output_button: gtk::MenuButton,
    pub output_list: gtk::ListBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SliderAccessibleOrientation {
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SliderAccessibilityPlan {
    playback_position_label: String,
    volume_label: String,
    orientation: SliderAccessibleOrientation,
}

fn slider_accessibility_plan(locale: &str) -> SliderAccessibilityPlan {
    SliderAccessibilityPlan {
        playback_position_label: rust_i18n::t!("header.playback_position", locale = locale)
            .into_owned(),
        volume_label: rust_i18n::t!("header.volume", locale = locale).into_owned(),
        orientation: SliderAccessibleOrientation::Horizontal,
    }
}

fn expose_slider_accessibility(progress: &gtk::Scale, volume: &gtk::Scale, locale: &str) {
    let plan = slider_accessibility_plan(locale);
    let orientation = match plan.orientation {
        SliderAccessibleOrientation::Horizontal => gtk::Orientation::Horizontal,
    };
    progress.update_property(&[
        gtk::accessible::Property::Label(&plan.playback_position_label),
        gtk::accessible::Property::Orientation(orientation),
    ]);
    volume.update_property(&[
        gtk::accessible::Property::Label(&plan.volume_label),
        gtk::accessible::Property::Orientation(orientation),
    ]);
}

/// Build the full header bar and return all interactive widgets.
pub fn build_header_bar() -> HeaderBarWidgets {
    // ── Left: Playback Controls ──────────────────────────────────────
    let btn_prev = gtk::Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .tooltip_text(rust_i18n::t!("header.previous").as_ref())
        .build();

    let btn_play = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text(rust_i18n::t!("header.play").as_ref())
        .css_classes(["suggested-action", "circular"])
        .build();

    let btn_next = gtk::Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .tooltip_text(rust_i18n::t!("header.next").as_ref())
        .build();

    let repeat_mode: Rc<Cell<RepeatMode>> = Rc::new(Cell::new(RepeatMode::Off));
    let btn_repeat = gtk::ToggleButton::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .tooltip_text(rust_i18n::t!("header.repeat_off").as_ref())
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
                RepeatMode::Off => (
                    "media-playlist-repeat-symbolic",
                    rust_i18n::t!("header.repeat_off"),
                    false,
                ),
                RepeatMode::All => (
                    "media-playlist-repeat-symbolic",
                    rust_i18n::t!("header.repeat_all"),
                    true,
                ),
                RepeatMode::One => (
                    "media-playlist-repeat-song-symbolic",
                    rust_i18n::t!("header.repeat_one"),
                    true,
                ),
            };
            btn.set_icon_name(icon);
            btn.set_tooltip_text(Some(tooltip.as_ref()));
            btn.set_active(active);
        });
    }

    let btn_shuffle = gtk::ToggleButton::builder()
        .icon_name("media-playlist-shuffle-symbolic")
        .tooltip_text(rust_i18n::t!("header.shuffle").as_ref())
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

    // GtkScale exposes its native value/range and keyboard controls. Give
    // each otherwise-unlabelled scale a stable, localized accessible name so
    // assistive technology can distinguish playback position from volume.
    expose_slider_accessibility(&progress, &volume_scale, &rust_i18n::locale());

    let volume_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .valign(Align::Center)
        .build();
    volume_box.append(&volume_icon);
    volume_box.append(&volume_scale);

    // ── Output selector (iTunes AirPlay-style) ───────────────────────
    // A MenuButton with a Popover containing a ListBox of output
    // destinations.  "My Computer" is always present; MPD sinks are
    // added/removed dynamically via the "+" button at the bottom.
    let output_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Default "My Computer" row — always present, always first.
    let local_row = build_output_row("My Computer", "audio-speakers-symbolic", true);
    output_list.append(&local_row);

    let add_output_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .tooltip_text(rust_i18n::t!("header.add_output").as_ref())
        .halign(Align::Center)
        .build();

    let output_popover_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    output_popover_box.append(&output_list);
    output_popover_box.append(&add_output_btn);

    let output_popover = gtk::Popover::builder().child(&output_popover_box).build();

    let output_button = gtk::MenuButton::builder()
        .icon_name("audio-speakers-symbolic")
        .popover(&output_popover)
        .tooltip_text(rust_i18n::t!("header.output").as_ref())
        .valign(Align::Center)
        .build();

    // Modern GNOME primary menu (Ptyxis-style)
    let menu = gtk::gio::Menu::new();
    let section1 = gtk::gio::Menu::new();
    let migrate_label = rust_i18n::t!("rhythmbox_migration.menu_action");
    section1.append(Some(migrate_label.as_ref()), Some("win.migrate-rhythmbox"));
    section1.append(Some("_Preferences"), Some("win.show-preferences"));
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
    header.pack_end(&output_button);
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
        output_button,
        output_list,
    }
}

// ── Output selector helpers ─────────────────────────────────────────────

/// Build a single row for the output selector popover.
///
/// Each row shows an icon, the output name, and a checkmark image that
/// is visible only for the currently active output.
pub fn build_output_row(name: &str, icon_name: &str, active: bool) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(4)
        .margin_end(4)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let icon = gtk::Image::builder().icon_name(icon_name).build();

    let label = gtk::Label::builder()
        .label(name)
        .hexpand(true)
        .halign(Align::Start)
        .build();

    let check = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .visible(active)
        .build();
    check.set_widget_name("output-check");

    row.append(&icon);
    row.append(&label);
    row.append(&check);
    row
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct AccessibilityCatalog {
        header: AccessibilityHeader,
    }

    #[derive(Debug, Deserialize)]
    struct AccessibilityHeader {
        playback_position: String,
        volume: String,
    }

    #[test]
    fn slider_accessibility_plan_is_backed_by_every_yaml_catalog() {
        let locale_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");

        for locale in rust_i18n::available_locales!() {
            let path = locale_dir.join(format!("{locale}.yml"));
            let yaml = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let catalog: AccessibilityCatalog = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            let plan = slider_accessibility_plan(&locale);

            assert_eq!(plan.orientation, SliderAccessibleOrientation::Horizontal);
            assert!(!catalog.header.playback_position.trim().is_empty());
            assert!(!catalog.header.volume.trim().is_empty());
            assert_eq!(
                plan.playback_position_label,
                catalog.header.playback_position,
                "header.playback_position fell back instead of using {}",
                path.display()
            );
            assert_eq!(
                plan.volume_label,
                catalog.header.volume,
                "header.volume fell back instead of using {}",
                path.display()
            );
            assert_ne!(
                plan.playback_position_label, plan.volume_label,
                "the two sliders are indistinguishable for {locale}"
            );
        }
    }
}
