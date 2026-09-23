//! The Preferences "Equalizer" group.
//!
//! The group edits `AppConfig::equalizer`. Every change applies to the
//! active output at once and is saved to `config.json` through the
//! Preferences save queue, after a short pause or when the dialog closes.
//! Outputs that cannot run the equalizer get the same controls, disabled,
//! with the reason as the group description.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;

use super::preferences::{AppConfig, ConfigSaveQueue};
use crate::audio::equalizer::{
    snap_gain_db, ClipProtection, EqualizerSettings, Preset, BAND_CENTERS_HZ, GAIN_STEP_DB,
    MAX_GAIN_DB, MIN_GAIN_DB,
};
use crate::audio::output::{AudioOutput, OutputType};

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
        Preset::Pop => rust_i18n::t!("equalizer.preset_pop"),
        Preset::Rock => rust_i18n::t!("equalizer.preset_rock"),
        Preset::Jazz => rust_i18n::t!("equalizer.preset_jazz"),
        Preset::Classical => rust_i18n::t!("equalizer.preset_classical"),
        Preset::Custom => rust_i18n::t!("equalizer.preset_custom"),
    }
    .into_owned()
}

/// "29 Hz" below 1 kHz, otherwise kilohertz to one decimal: "1.9 kHz", "15 kHz".
fn frequency_label(hz: u32) -> String {
    if hz < 1000 {
        return rust_i18n::t!("equalizer.hz", value = hz).into_owned();
    }
    let khz = format!("{:.1}", f64::from(hz) / 1000.0);
    let khz = khz.strip_suffix(".0").unwrap_or(&khz);
    rust_i18n::t!("equalizer.khz", value = khz).into_owned()
}

fn gain_label(db: f64) -> String {
    rust_i18n::t!("equalizer.gain_db", value = format!("{db:+.1}")).into_owned()
}

fn position(preset: Preset) -> u32 {
    Preset::ALL
        .iter()
        .position(|candidate| *candidate == preset)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

/// A gain slider row. The slider is labelled with the row title for
/// assistive technology and draws its value as localized dB text.
fn gain_row(title: &str, db: f64) -> (adw::ActionRow, gtk::Scale) {
    let scale = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        MIN_GAIN_DB,
        MAX_GAIN_DB,
        GAIN_STEP_DB,
    );
    scale.set_value(db);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Left);
    scale.set_width_request(240);
    scale.add_mark(0.0, gtk::PositionType::Bottom, None);
    scale.set_format_value_func(|_, value| gain_label(value));
    scale.update_property(&[gtk::accessible::Property::Label(title)]);
    let row = adw::ActionRow::builder().title(title).build();
    row.add_suffix(&scale);
    (row, scale)
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

/// The preamp row followed by the band rows, with their sliders.
fn gain_rows(settings: &EqualizerSettings) -> (Vec<gtk::Widget>, gtk::Scale, Vec<gtk::Scale>) {
    let (preamp_row, preamp) = gain_row(&rust_i18n::t!("equalizer.preamp"), settings.preamp_db);
    let (band_rows, bands): (Vec<_>, Vec<_>) = BAND_CENTERS_HZ
        .iter()
        .zip(settings.bands_db)
        .map(|(hz, db)| gain_row(&frequency_label(*hz), db))
        .unzip();
    let rows = std::iter::once(preamp_row)
        .chain(band_rows)
        .map(Cast::upcast)
        .collect();
    (rows, preamp, bands)
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
    let (gain_rows, preamp, bands) = gain_rows(&settings);
    let clip_protection = clip_protection_row(settings.clip_protection);
    let reset = reset_button();

    let mut controls: Vec<gtk::Widget> = vec![enabled.clone().upcast(), preset.clone().upcast()];
    controls.extend(gain_rows);
    controls.extend([clip_protection.clone().upcast(), reset.clone().upcast()]);
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
                "29 Hz", "59 Hz", "119 Hz", "237 Hz", "474 Hz", "947 Hz", "1.9 kHz", "3.8 kHz",
                "7.5 kHz", "15 kHz"
            ]
        );
        assert_eq!(gain_label(-1.5), "-1.5 dB");
        assert_eq!(gain_label(3.0), "+3.0 dB");
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
        assert_eq!(panel.bands[2].value(), 3.0);
        assert!(panel.preamp.is_sensitive());

        panel.preset.set_selected(position(Preset::Rock));
        let rock = last(&changes);
        assert_eq!(rock.preset, Preset::Rock);
        assert_eq!(rock.bands_db, Preset::Rock.band_gains_db().unwrap());
        assert_eq!(panel.bands[0].value(), 3.0, "sliders follow the preset");
        assert_eq!(panel.preamp.value(), -1.0);
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
