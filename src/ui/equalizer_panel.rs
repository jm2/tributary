//! The Preferences "Equalizer" group.
//!
//! The group edits `AppConfig::equalizer`. Every change applies to the
//! active output at once and is saved to `config.json` through the
//! Preferences save queue, after a short pause or when the dialog closes.
//! Outputs that cannot run the equalizer get the same controls, disabled,
//! with the reason as the group description.
//!
//! The gains are a graphic equalizer, as in iTunes and Winamp: a row of
//! vertical sliders, the preamp first and then the ten bands, boost at the
//! top. The mouse wheel over the sliders scrolls the page instead of moving
//! a slider; dragging and the keyboard still move them.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, glib};

use super::preferences::{AppConfig, ConfigSaveQueue};
use crate::audio::equalizer::{
    snap_gain_db, ClipProtection, EqualizerSettings, Preset, BAND_CENTERS_HZ, GAIN_STEP_DB,
    MAX_GAIN_DB, MIN_GAIN_DB,
};
use crate::audio::output::{AudioOutput, OutputType};

/// Height of the slider troughs, in pixels.
const SLIDER_HEIGHT: i32 = 160;

/// Build the group editing `config` and driving `output`.
pub fn preferences_group(
    config: &Rc<RefCell<AppConfig>>,
    saves: &ConfigSaveQueue,
    output: &Rc<RefCell<Box<dyn AudioOutput>>>,
) -> adw::PreferencesGroup {
    let on_change = {
        let config = config.clone();
        let saves = saves.clone();
        let output = output.clone();
        Rc::new(move |settings: &EqualizerSettings| {
            output.borrow().set_equalizer(settings);
            config.borrow_mut().equalizer = *settings;
            saves.schedule();
        })
    };
    let unavailable = unavailable_reason(output.borrow().as_ref());
    let settings = config.borrow().equalizer;
    build(settings, unavailable.as_deref(), on_change).0
}

/// Why `output` cannot run the equalizer, or `None` when it can.
fn unavailable_reason(output: &dyn AudioOutput) -> Option<String> {
    if output.supports_equalizer() {
        return None;
    }
    let reason = match output.output_type() {
        OutputType::Local => rust_i18n::t!("equalizer.unavailable"),
        OutputType::AirPlay => rust_i18n::t!("equalizer.unsupported.airplay"),
        OutputType::Chromecast => rust_i18n::t!("equalizer.unsupported.chromecast"),
        OutputType::Mpd => rust_i18n::t!("equalizer.unsupported.mpd"),
    };
    Some(reason.into_owned())
}

fn preset_label(preset: Preset) -> String {
    match preset {
        Preset::Flat => rust_i18n::t!("equalizer.preset_flat"),
        Preset::Classical => rust_i18n::t!("equalizer.preset_classical"),
        Preset::Club => rust_i18n::t!("equalizer.preset_club"),
        Preset::Dance => rust_i18n::t!("equalizer.preset_dance"),
        Preset::FullBass => rust_i18n::t!("equalizer.preset_full_bass"),
        Preset::FullBassTreble => rust_i18n::t!("equalizer.preset_full_bass_treble"),
        Preset::FullTreble => rust_i18n::t!("equalizer.preset_full_treble"),
        Preset::Headphones => rust_i18n::t!("equalizer.preset_headphones"),
        Preset::LargeHall => rust_i18n::t!("equalizer.preset_large_hall"),
        Preset::Live => rust_i18n::t!("equalizer.preset_live"),
        Preset::Party => rust_i18n::t!("equalizer.preset_party"),
        Preset::Pop => rust_i18n::t!("equalizer.preset_pop"),
        Preset::Reggae => rust_i18n::t!("equalizer.preset_reggae"),
        Preset::Rock => rust_i18n::t!("equalizer.preset_rock"),
        Preset::Ska => rust_i18n::t!("equalizer.preset_ska"),
        Preset::Soft => rust_i18n::t!("equalizer.preset_soft"),
        Preset::SoftRock => rust_i18n::t!("equalizer.preset_soft_rock"),
        Preset::Techno => rust_i18n::t!("equalizer.preset_techno"),
        Preset::Custom => rust_i18n::t!("equalizer.preset_custom"),
    }
    .into_owned()
}

/// "32 Hz" below 1 kHz, otherwise kilohertz to one decimal: "1 kHz", "16 kHz".
/// Names a band slider for assistive technology.
fn frequency_label(hz: u32) -> String {
    if hz < 1000 {
        return rust_i18n::t!("equalizer.hz", value = hz).into_owned();
    }
    let khz = format!("{:.1}", f64::from(hz) / 1000.0);
    let khz = khz.strip_suffix(".0").unwrap_or(&khz);
    rust_i18n::t!("equalizer.khz", value = khz).into_owned()
}

/// The short caption under a band slider, as iTunes and Winamp print it:
/// "32", "125", "1K", "16K".
fn short_frequency_label(hz: u32) -> String {
    if hz < 1000 {
        hz.to_string()
    } else {
        format!("{}K", hz / 1000)
    }
}

/// A slider value: "+3.0 dB", "-1.5 dB".
fn gain_label(db: f64) -> String {
    rust_i18n::t!("equalizer.gain_db", value = format!("{db:+.1}")).into_owned()
}

/// A whole-dB scale mark: "+12 dB", "0 dB", "-12 dB".
fn scale_label(db: f64) -> String {
    let value = if db.abs() < GAIN_STEP_DB / 2.0 {
        "0".to_owned()
    } else {
        format!("{db:+.0}")
    };
    rust_i18n::t!("equalizer.gain_db", value = value).into_owned()
}

fn position(preset: Preset) -> u32 {
    Preset::ALL
        .iter()
        .position(|candidate| *candidate == preset)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

/// A vertical gain slider, boost at the top, with a mark at 0 dB. It is
/// named `title` for assistive technology and reports its value as dB text,
/// which its tooltip also shows.
fn gain_slider(title: &str, db: f64) -> gtk::Scale {
    let scale = gtk::Scale::with_range(
        gtk::Orientation::Vertical,
        MIN_GAIN_DB,
        MAX_GAIN_DB,
        GAIN_STEP_DB,
    );
    scale.set_inverted(true);
    scale.set_draw_value(false);
    // No fill from the bottom: 0 dB, not -12 dB, is the neutral position.
    scale.set_has_origin(false);
    scale.set_height_request(SLIDER_HEIGHT);
    scale.set_hexpand(true);
    scale.set_halign(gtk::Align::Center);
    scale.add_mark(0.0, gtk::PositionType::Right, None);
    scale.update_property(&[gtk::accessible::Property::Label(title)]);
    scale.set_value(db);
    show_gain(&scale);
    scale.connect_value_changed(show_gain);
    scale
}

/// Show `scale`'s value in its tooltip and accessible value text.
fn show_gain(scale: &gtk::Scale) {
    let text = gain_label(scale.value());
    scale.set_tooltip_text(Some(&text));
    scale.update_property(&[gtk::accessible::Property::ValueText(&text)]);
}

/// A small caption for the slider row, hidden from assistive technology,
/// which reads each slider's own name and value instead.
fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .css_classes(["caption", "dim-label"])
        .justify(gtk::Justification::Center)
        .build();
    label.set_accessible_role(gtk::AccessibleRole::Presentation);
    label
}

/// The "+12 dB / 0 dB / -12 dB" axis beside the sliders.
fn gain_axis() -> gtk::CenterBox {
    let axis = gtk::CenterBox::builder()
        .orientation(gtk::Orientation::Vertical)
        .height_request(SLIDER_HEIGHT)
        .build();
    let mark = |db: f64| {
        let label = caption(&scale_label(db));
        label.set_xalign(1.0);
        label
    };
    axis.set_start_widget(Some(&mark(MAX_GAIN_DB)));
    axis.set_center_widget(Some(&mark(0.0)));
    axis.set_end_widget(Some(&mark(MIN_GAIN_DB)));
    axis.set_accessible_role(gtk::AccessibleRole::Presentation);
    axis
}

/// The graphic equalizer: a gain axis, the preamp slider, a separator, and
/// the ten band sliders, each above its caption.
fn slider_grid(settings: &EqualizerSettings) -> (gtk::Grid, gtk::Scale, Vec<gtk::Scale>) {
    let grid = gtk::Grid::builder()
        .css_classes(["equalizer-sliders"])
        .column_spacing(2)
        .row_spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    grid.attach(&gain_axis(), 0, 0, 1, 1);

    let preamp_title = rust_i18n::t!("equalizer.preamp");
    let preamp = gain_slider(&preamp_title, settings.preamp_db);
    grid.attach(&preamp, 1, 0, 1, 1);
    let preamp_caption = caption(&preamp_title);
    preamp_caption.set_wrap(true);
    preamp_caption.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    preamp_caption.set_max_width_chars(8);
    grid.attach(&preamp_caption, 1, 1, 1, 1);

    let separator = gtk::Separator::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_start(6)
        .margin_end(6)
        .build();
    grid.attach(&separator, 2, 0, 1, 2);

    let mut bands = Vec::with_capacity(BAND_CENTERS_HZ.len());
    for (column, (hz, db)) in (3..).zip(BAND_CENTERS_HZ.iter().zip(settings.bands_db)) {
        let band = gain_slider(&frequency_label(*hz), db);
        grid.attach(&band, column, 0, 1, 1);
        grid.attach(&caption(&short_frequency_label(*hz)), column, 1, 1, 1);
        bands.push(band);
    }
    route_wheel_to_page(&grid);
    (grid, preamp, bands)
}

/// Make the mouse wheel over `sliders` scroll the enclosing page instead of
/// moving a slider.
///
/// A capture-phase controller on the container sees every scroll before the
/// sliders do. It moves the nearest [`gtk::ScrolledWindow`] ancestor as that
/// window would scroll itself, then stops the event, so the sliders never
/// receive wheel or touchpad scrolling. Drags and keys are not scroll events
/// and still reach them.
fn route_wheel_to_page(sliders: &impl IsA<gtk::Widget>) {
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_scroll(|controller, _, dy| {
        let window = controller
            .widget()
            .and_then(|widget| widget.ancestor(gtk::ScrolledWindow::static_type()))
            .and_downcast::<gtk::ScrolledWindow>();
        if let Some(window) = window {
            scroll_by(&window.vadjustment(), dy, controller.unit());
        }
        glib::Propagation::Stop
    });
    sliders.add_controller(controller);
}

/// Move `adjustment` by a scroll of `delta` in `unit`, the distance GTK
/// 4.22's `GtkScrolledWindow` uses for the same scroll: a wheel notch moves
/// the 2/3 power of the page size, and touchpad (surface) deltas are scaled
/// by 2.5.
fn scroll_by(adjustment: &gtk::Adjustment, delta: f64, unit: gdk::ScrollUnit) {
    let distance = if unit == gdk::ScrollUnit::Wheel {
        delta * adjustment.page_size().powf(2.0 / 3.0)
    } else {
        delta * 2.5
    };
    adjustment.set_value(adjustment.value() + distance);
}

/// The group's controls and the settings they show.
pub struct Panel {
    settings: Cell<EqualizerSettings>,
    /// Set while [`Panel::show`] moves widgets, so their handlers ignore it.
    syncing: Cell<bool>,
    on_change: Rc<dyn Fn(&EqualizerSettings)>,
    pub enabled: adw::SwitchRow,
    pub preset: adw::ComboRow,
    pub preamp: gtk::Scale,
    pub bands: Vec<gtk::Scale>,
    pub clip_protection: adw::ComboRow,
    pub reset: gtk::Button,
}

impl Panel {
    /// Apply a user edit: update the settings, re-sync the widgets it
    /// affects, and report the result.
    fn edit(&self, change: impl FnOnce(&mut EqualizerSettings)) {
        if self.syncing.get() {
            return;
        }
        let mut settings = self.settings.get();
        change(&mut settings);
        self.settings.set(settings);
        self.show(&settings);
        (self.on_change)(&settings);
    }

    fn show(&self, settings: &EqualizerSettings) {
        self.syncing.set(true);
        self.preset.set_selected(position(settings.preset));
        self.preamp.set_value(settings.preamp_db);
        for (scale, db) in self.bands.iter().zip(settings.bands_db) {
            scale.set_value(db);
        }
        self.syncing.set(false);
    }

    /// Hand-edit one gain; the preset becomes Custom.
    fn edit_gain(&self, scale: &gtk::Scale, band: Option<usize>) {
        let db = snap_gain_db(scale.value());
        self.edit(|settings| {
            match band {
                Some(index) => settings.bands_db[index] = db,
                None => settings.preamp_db = db,
            }
            settings.preset = Preset::Custom;
        });
    }
}

fn combo_row(title: &str, labels: &[String], selected: u32) -> adw::ComboRow {
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&labels))
        .selected(selected)
        .build()
}

/// The group holding `controls`, all disabled with the reason as the
/// description when the equalizer is `unavailable`.
fn group(controls: &[gtk::Widget], unavailable: Option<&str>) -> adw::PreferencesGroup {
    let description = unavailable.map_or_else(
        || rust_i18n::t!("equalizer.description").into_owned(),
        str::to_owned,
    );
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("equalizer.title").as_ref())
        .description(description)
        .build();
    for control in controls {
        group.add(control);
        control.set_sensitive(unavailable.is_none());
    }
    group
}

/// The boxed-list row holding the slider grid. It is not activatable, so
/// clicks go to the sliders.
fn slider_row(grid: &gtk::Grid) -> adw::PreferencesRow {
    adw::PreferencesRow::builder()
        .title(rust_i18n::t!("equalizer.title").as_ref())
        .activatable(false)
        .selectable(false)
        .focusable(false)
        .child(grid)
        .build()
}

fn clip_protection_row(protection: ClipProtection) -> adw::ComboRow {
    combo_row(
        &rust_i18n::t!("equalizer.clip_protection"),
        &[
            rust_i18n::t!("equalizer.clip_off").into_owned(),
            rust_i18n::t!("equalizer.clip_soft").into_owned(),
        ],
        u32::from(protection == ClipProtection::Soft),
    )
}

fn reset_button() -> gtk::Button {
    gtk::Button::builder()
        .label(rust_i18n::t!("equalizer.reset_flat").as_ref())
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .margin_top(4)
        .build()
}

/// Build the group showing `settings`. Edits call `on_change` with the new
/// settings; `unavailable` disables every control and explains why.
pub fn build(
    settings: EqualizerSettings,
    unavailable: Option<&str>,
    on_change: Rc<dyn Fn(&EqualizerSettings)>,
) -> (adw::PreferencesGroup, Rc<Panel>) {
    let enabled = adw::SwitchRow::builder()
        .title(rust_i18n::t!("equalizer.enabled").as_ref())
        .active(settings.enabled)
        .build();
    let preset = combo_row(
        &rust_i18n::t!("equalizer.preset"),
        &Preset::ALL.map(preset_label),
        position(settings.preset),
    );
    let (sliders, preamp, bands) = slider_grid(&settings);
    let clip_protection = clip_protection_row(settings.clip_protection);
    let reset = reset_button();

    let controls: [gtk::Widget; 5] = [
        enabled.clone().upcast(),
        preset.clone().upcast(),
        slider_row(&sliders).upcast(),
        clip_protection.clone().upcast(),
        reset.clone().upcast(),
    ];
    let group = group(&controls, unavailable);
    let panel = Rc::new(Panel {
        settings: Cell::new(settings),
        syncing: Cell::new(false),
        on_change,
        enabled,
        preset,
        preamp,
        bands,
        clip_protection,
        reset,
    });
    connect(&panel, &Rc::downgrade(&panel));
    // The handlers reach the panel weakly and the group owns it, so the panel
    // and its widgets are freed with the dialog instead of keeping each
    // other alive. Once the group is torn down, widget notifications are
    // no longer edits.
    let owner = panel.clone();
    group.connect_destroy(move |_| owner.syncing.set(true));
    (group, panel)
}

/// Run `action` on the panel unless its group is gone.
fn with(panel: &Weak<Panel>, action: impl FnOnce(&Panel)) {
    if let Some(panel) = panel.upgrade() {
        action(&panel);
    }
}

fn connect(widgets: &Panel, panel: &Weak<Panel>) {
    let p = panel.clone();
    widgets.enabled.connect_active_notify(move |row| {
        let active = row.is_active();
        with(&p, |panel| panel.edit(|settings| settings.enabled = active));
    });
    let p = panel.clone();
    widgets.preset.connect_selected_notify(move |row| {
        let preset = Preset::ALL
            .get(row.selected() as usize)
            .copied()
            .unwrap_or_default();
        with(&p, |panel| {
            panel.edit(|settings| settings.select_preset(preset));
        });
    });
    let p = panel.clone();
    widgets
        .preamp
        .connect_value_changed(move |scale| with(&p, |panel| panel.edit_gain(scale, None)));
    for (index, band) in widgets.bands.iter().enumerate() {
        let p = panel.clone();
        band.connect_value_changed(move |scale| {
            with(&p, |panel| panel.edit_gain(scale, Some(index)));
        });
    }
    let p = panel.clone();
    widgets.clip_protection.connect_selected_notify(move |row| {
        let protection = if row.selected() == 1 {
            ClipProtection::Soft
        } else {
            ClipProtection::Off
        };
        with(&p, |panel| {
            panel.edit(|settings| settings.clip_protection = protection);
        });
    });
    let p = panel.clone();
    widgets.reset.connect_clicked(move |_| {
        with(&p, |panel| {
            panel.edit(|settings| settings.select_preset(Preset::Flat));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequencies_read_as_hertz_below_one_kilohertz_and_kilohertz_above() {
        let labels: Vec<String> = BAND_CENTERS_HZ
            .iter()
            .map(|hz| frequency_label(*hz))
            .collect();
        assert_eq!(
            labels,
            [
                "32 Hz", "64 Hz", "125 Hz", "250 Hz", "500 Hz", "1 kHz", "2 kHz", "4 kHz", "8 kHz",
                "16 kHz"
            ]
        );
        assert_eq!(frequency_label(1500), "1.5 kHz");
        let captions: Vec<String> = BAND_CENTERS_HZ
            .iter()
            .map(|hz| short_frequency_label(*hz))
            .collect();
        assert_eq!(
            captions,
            ["32", "64", "125", "250", "500", "1K", "2K", "4K", "8K", "16K"]
        );
        assert_eq!(gain_label(-1.5), "-1.5 dB");
        assert_eq!(gain_label(3.0), "+3.0 dB");
        assert_eq!(
            [MAX_GAIN_DB, 0.0, MIN_GAIN_DB].map(scale_label),
            ["+12 dB", "0 dB", "-12 dB"]
        );
    }

    #[test]
    fn every_preset_has_its_own_name() {
        let names: Vec<String> = Preset::ALL.into_iter().map(preset_label).collect();
        assert_eq!(
            names,
            [
                "Flat",
                "Classical",
                "Club",
                "Dance",
                "Full Bass",
                "Full Bass & Treble",
                "Full Treble",
                "Headphones",
                "Large Hall",
                "Live",
                "Party",
                "Pop",
                "Reggae",
                "Rock",
                "Ska",
                "Soft",
                "Soft Rock",
                "Techno",
                "Custom",
            ]
        );
    }

    #[test]
    fn every_catalog_translates_every_equalizer_key() {
        use std::collections::BTreeMap;

        fn flatten(prefix: &str, value: &serde_yaml::Value, out: &mut BTreeMap<String, String>) {
            match value {
                serde_yaml::Value::Mapping(map) => {
                    for (key, value) in map {
                        let key = format!("{prefix}.{}", key.as_str().unwrap());
                        flatten(&key, value, out);
                    }
                }
                serde_yaml::Value::String(text) => {
                    out.insert(prefix.to_owned(), text.clone());
                }
                other => panic!("{prefix} is not text: {other:?}"),
            }
        }
        fn placeholders(text: &str) -> Vec<&str> {
            text.split("%{")
                .skip(1)
                .filter_map(|rest| rest.split('}').next())
                .collect()
        }
        let catalog = |path: &std::path::Path| {
            let yaml: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let mut keys = BTreeMap::new();
            flatten("equalizer", &yaml["equalizer"], &mut keys);
            keys
        };

        let locales = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let english = catalog(&locales.join("en.yml"));
        let mut checked = 0;
        for entry in std::fs::read_dir(&locales).unwrap() {
            let path = entry.unwrap().path();
            let translated = catalog(&path);
            assert_eq!(
                translated.keys().collect::<Vec<_>>(),
                english.keys().collect::<Vec<_>>(),
                "{}",
                path.display()
            );
            for (key, text) in &translated {
                assert!(!text.trim().is_empty(), "{} {key}", path.display());
                assert_eq!(
                    placeholders(text),
                    placeholders(&english[key]),
                    "{} {key}",
                    path.display()
                );
            }
            checked += 1;
        }
        assert_eq!(checked, 13);
    }
}

#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::*;

    type Changes = Rc<RefCell<Vec<EqualizerSettings>>>;

    fn panel(settings: EqualizerSettings) -> (adw::PreferencesGroup, Rc<Panel>, Changes) {
        let changes: Changes = Rc::default();
        let recorded = changes.clone();
        let (group, panel) = build(
            settings,
            None,
            Rc::new(move |settings: &EqualizerSettings| recorded.borrow_mut().push(*settings)),
        );
        (group, panel, changes)
    }

    fn last(changes: &Changes) -> EqualizerSettings {
        *changes
            .borrow()
            .last()
            .expect("an edit reported its settings")
    }

    #[allow(clippy::float_cmp)] // preset tables and snapped gains are exact half-dB steps
    pub fn equalizer_panel_edits_report_consistent_settings() {
        let mut saved = EqualizerSettings {
            enabled: true,
            ..EqualizerSettings::default()
        };
        saved.select_preset(Preset::Pop);
        let (_group, panel, changes) = panel(saved);
        assert!(panel.enabled.is_active());
        assert_eq!(panel.preset.selected(), position(Preset::Pop));
        assert_eq!(panel.bands[3].value(), 4.0);
        assert_eq!(panel.preamp.value(), -4.5);
        assert!(panel.preamp.is_sensitive());

        panel.preset.set_selected(position(Preset::Rock));
        let rock = last(&changes);
        assert_eq!(rock.preset, Preset::Rock);
        assert_eq!(rock.bands_db, Preset::Rock.band_gains_db().unwrap());
        assert_eq!(panel.bands[0].value(), 5.0, "sliders follow the preset");
        assert_eq!(panel.preamp.value(), -6.5);
        assert_eq!(
            changes.borrow().len(),
            1,
            "moving the sliders is not an edit"
        );

        panel.bands[9].set_value(4.3);
        let edited = last(&changes);
        assert_eq!(edited.bands_db[9], 4.5, "gains snap to half-dB steps");
        assert_eq!(edited.preset, Preset::Custom);
        assert_eq!(panel.preset.selected(), position(Preset::Custom));
        assert_eq!(panel.bands[9].value(), 4.5);

        panel.clip_protection.set_selected(1);
        assert_eq!(last(&changes).clip_protection, ClipProtection::Soft);
        panel.enabled.set_active(false);
        assert!(!last(&changes).enabled);

        panel.reset.emit_clicked();
        let flat = last(&changes);
        assert_eq!(flat.preset, Preset::Flat);
        assert_eq!(flat.bands_db, [0.0; 10]);
        assert_eq!(flat.preamp_db, 0.0);
        assert_eq!(
            flat.clip_protection,
            ClipProtection::Soft,
            "reset keeps protection"
        );
        assert!(!flat.enabled, "reset keeps the switch");
        assert!(panel.bands.iter().all(|scale| scale.value() == 0.0));
    }

    /// The grid holding the sliders, which carries the wheel controller.
    fn sliders(panel: &Panel) -> gtk::Grid {
        panel
            .preamp
            .parent()
            .and_downcast::<gtk::Grid>()
            .expect("the sliders share one grid")
    }

    /// The text of the caption at `column`, `row` of the slider grid.
    fn caption_at(grid: &gtk::Grid, column: i32, row: i32) -> String {
        grid.child_at(column, row)
            .and_downcast::<gtk::Label>()
            .unwrap_or_else(|| panic!("no caption at {column}, {row}"))
            .label()
            .into()
    }

    #[allow(clippy::float_cmp)] // slider bounds and snapped gains are exact half-dB steps
    pub fn equalizer_sliders_stand_like_a_graphic_equalizer() {
        let mut saved = EqualizerSettings::default();
        saved.select_preset(Preset::FullTreble);
        let (_group, panel, _changes) = panel(saved);
        let grid = &sliders(&panel);

        // Preamp first, then a separator, then the ten bands left to right,
        // each above its short frequency caption.
        assert_eq!(
            grid.child_at(1, 0).as_ref(),
            Some(panel.preamp.upcast_ref())
        );
        assert_eq!(caption_at(grid, 1, 1), "Preamp");
        assert!(grid
            .child_at(2, 0)
            .and_downcast::<gtk::Separator>()
            .is_some());
        let captions: Vec<String> = (3..13).map(|column| caption_at(grid, column, 1)).collect();
        assert_eq!(
            captions,
            ["32", "64", "125", "250", "500", "1K", "2K", "4K", "8K", "16K"]
        );
        for (column, band) in (3..).zip(&panel.bands) {
            assert_eq!(grid.child_at(column, 0).as_ref(), Some(band.upcast_ref()));
        }
        let axis = grid
            .child_at(0, 0)
            .and_downcast::<gtk::CenterBox>()
            .unwrap();
        let marks: Vec<String> = [axis.start_widget(), axis.center_widget(), axis.end_widget()]
            .into_iter()
            .map(|mark| mark.and_downcast::<gtk::Label>().unwrap().label().into())
            .collect();
        assert_eq!(marks, ["+12 dB", "0 dB", "-12 dB"]);
        assert_eq!(axis.accessible_role(), gtk::AccessibleRole::Presentation);

        for scale in std::iter::once(&panel.preamp).chain(&panel.bands) {
            assert_eq!(scale.orientation(), gtk::Orientation::Vertical);
            assert!(scale.is_inverted(), "boost is at the top");
            assert!(!scale.draws_value());
            let range = scale.adjustment();
            assert_eq!((range.lower(), range.upper()), (-12.0, 12.0));
            assert!(gtk::test_accessible_has_property(
                scale,
                gtk::AccessibleProperty::Label
            ));
            assert!(gtk::test_accessible_has_property(
                scale,
                gtk::AccessibleProperty::ValueText
            ));
        }
        assert_eq!(panel.bands[9].value(), 10.0);
        assert_eq!(panel.bands[9].tooltip_text().as_deref(), Some("+10.0 dB"));
        assert_eq!(panel.preamp.tooltip_text().as_deref(), Some("-10.0 dB"));
        assert_eq!(
            grid.child_at(5, 1).unwrap().accessible_role(),
            gtk::AccessibleRole::Presentation,
            "screen readers read each slider's own name, not the caption"
        );
    }

    #[allow(clippy::float_cmp)] // snapped gains are exact half-dB steps
    pub fn equalizer_sliders_follow_the_keyboard() {
        let (_group, panel, changes) = panel(EqualizerSettings::default());
        panel.bands[3].emit_by_name::<()>("move-slider", &[&gtk::ScrollType::StepUp]);
        let raised = last(&changes);
        assert_eq!(raised.bands_db[3], GAIN_STEP_DB, "Up raises the band");
        assert_eq!(raised.preset, Preset::Custom);
        assert_eq!(
            panel.bands[3].tooltip_text().as_deref(),
            Some("+0.5 dB"),
            "the tooltip follows the value"
        );
        panel
            .preamp
            .emit_by_name::<()>("move-slider", &[&gtk::ScrollType::StepDown]);
        assert_eq!(
            last(&changes).preamp_db,
            -GAIN_STEP_DB,
            "Down lowers the preamp"
        );
    }

    /// The capture-phase scroll controller on the slider grid.
    fn wheel_controller(sliders: &gtk::Grid) -> gtk::EventControllerScroll {
        let controllers = sliders.observe_controllers();
        let mut found = (0..controllers.n_items())
            .filter_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::EventControllerScroll>()
            })
            .filter(|controller| controller.propagation_phase() == gtk::PropagationPhase::Capture);
        let controller = found.next().expect("a capture-phase scroll controller");
        assert!(found.next().is_none(), "one wheel controller");
        controller
    }

    #[allow(clippy::float_cmp)] // the page offsets are computed the same way on both sides
    pub fn the_wheel_over_the_sliders_scrolls_the_page_not_a_slider() {
        let mut saved = EqualizerSettings::default();
        saved.select_preset(Preset::Rock);
        let (group, panel, changes) = panel(saved);
        let page = gtk::ScrolledWindow::new();
        page.set_child(Some(&group));
        let adjustment = page.vadjustment();
        adjustment.configure(0.0, 0.0, 1000.0, 10.0, 90.0, 100.0);
        let controller = wheel_controller(&sliders(&panel));
        let notch = 100.0_f64.powf(2.0 / 3.0);

        let handled: bool = controller.emit_by_name("scroll", &[&0.0_f64, &2.0_f64]);
        assert!(handled, "the wheel event stops before the sliders");
        assert_eq!(adjustment.value(), 2.0 * notch, "two notches down the page");
        let handled: bool = controller.emit_by_name("scroll", &[&0.0_f64, &-5.0_f64]);
        assert!(handled);
        assert_eq!(adjustment.value(), 0.0, "scrolling up stops at the top");
        let handled: bool = controller.emit_by_name("scroll", &[&3.0_f64, &0.0_f64]);
        assert!(handled, "sideways scrolling never reaches a slider either");

        assert!(changes.borrow().is_empty(), "no slider moved");
        assert_eq!(
            panel
                .bands
                .iter()
                .map(gtk::Scale::value)
                .collect::<Vec<_>>(),
            Preset::Rock.band_gains_db().unwrap()
        );

        // Outside a scrolled window the wheel still never moves a slider.
        page.set_child(None::<&gtk::Widget>);
        let handled: bool = controller.emit_by_name("scroll", &[&0.0_f64, &1.0_f64]);
        assert!(handled);
        assert!(changes.borrow().is_empty());
    }

    pub fn equalizer_panel_is_disabled_for_unsupported_outputs() {
        let (group, panel) = build(
            EqualizerSettings::default(),
            Some("MPD renders audio on the server."),
            Rc::new(|_: &EqualizerSettings| {}),
        );
        assert_eq!(
            group.description().as_deref(),
            Some("MPD renders audio on the server.")
        );
        assert!(!panel.enabled.is_sensitive());
        assert!(!panel.preset.is_sensitive());
        assert!(!panel.clip_protection.is_sensitive());
        assert!(!panel.reset.is_sensitive());
        assert!(!panel.preamp.is_sensitive());
        assert!(panel.bands.iter().all(|scale| !scale.is_sensitive()));

        let weak = Rc::downgrade(&panel);
        drop(panel);
        drop(group);
        assert!(weak.upgrade().is_none(), "the panel outlived its group");
    }
}
