use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use gstreamer as gst;
use gtk::glib;

use super::*;
use crate::ui::preferences::AppConfig;

/// RMS level in dBFS of a sine with amplitude 0.25, the tone most tests use.
const TONE_RMS_DB: f64 = -15.05;

/// Centre of band 5, the band the tone tests boost.
const TONE_HZ: f64 = 1000.0;

#[allow(clippy::float_cmp)] // preset tables and snapped gains are exact half-dB steps
#[test]
fn presets_load_their_gains_and_custom_keeps_the_current_ones() {
    let mut settings = EqualizerSettings::default();
    assert!(!settings.enabled);
    assert_eq!(settings.preset, Preset::Flat);
    assert_eq!(settings.clip_protection, ClipProtection::Off);

    settings.select_preset(Preset::Rock);
    assert_eq!(settings.bands_db, Preset::Rock.band_gains_db().unwrap());
    assert_eq!(settings.preamp_db, -1.0);

    settings.bands_db[0] = 4.5;
    settings.select_preset(Preset::Custom);
    assert_eq!(settings.preset, Preset::Custom);
    assert_eq!(
        settings.bands_db[0], 4.5,
        "Custom must keep the edited gains"
    );
    assert_eq!(settings.preamp_db, -1.0);

    for preset in Preset::ALL {
        let mut selected = EqualizerSettings::default();
        selected.select_preset(preset);
        assert_eq!(
            selected.validated().preset,
            preset,
            "{preset:?} is self-consistent"
        );
    }
}

#[allow(clippy::float_cmp)] // the range is exact
#[test]
fn the_bands_are_iso_octaves_with_twelve_db_either_way() {
    assert_eq!(
        BAND_CENTERS_HZ,
        [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]
    );
    assert_eq!(
        std::array::from_fn::<u32, 10, _>(band_width_hz),
        [32, 32, 61, 125, 250, 500, 1000, 2000, 4000, 8000]
    );
    assert_eq!((MIN_GAIN_DB, MAX_GAIN_DB), (-12.0, 12.0));
}

#[allow(clippy::float_cmp)] // snapped gains are exact half-dB steps
#[test]
fn validation_clamps_snaps_and_relabels_edited_presets() {
    assert_eq!(snap_gain_db(1.26), 1.5);
    assert_eq!(snap_gain_db(1.24), 1.0);
    assert_eq!(snap_gain_db(-0.2), 0.0);
    assert_eq!(snap_gain_db(12.2), MAX_GAIN_DB);
    assert_eq!(snap_gain_db(-24.0), MIN_GAIN_DB);
    assert_eq!(snap_gain_db(40.0), MAX_GAIN_DB);
    assert_eq!(snap_gain_db(-40.0), MIN_GAIN_DB);
    assert_eq!(snap_gain_db(f64::NAN), 0.0);

    let mut rock = EqualizerSettings::default();
    rock.select_preset(Preset::Rock);
    rock.bands_db[0] = 3.1; // snaps back onto the preset value
    assert_eq!(rock.validated().preset, Preset::Rock);

    let mut edited = rock;
    edited.bands_db[9] = 20.0;
    let edited = edited.validated();
    assert_eq!(edited.preset, Preset::Custom);
    assert_eq!(edited.bands_db[9], MAX_GAIN_DB);

    let mut preamp_edited = rock;
    preamp_edited.preamp_db = -1.5;
    assert_eq!(preamp_edited.validated().preset, Preset::Custom);
}

#[test]
fn the_config_field_round_trips_and_a_malformed_block_resets_only_the_equalizer() {
    let mut settings = EqualizerSettings {
        enabled: true,
        clip_protection: ClipProtection::Soft,
        ..EqualizerSettings::default()
    };
    settings.select_preset(Preset::Jazz);
    let config = AppConfig {
        equalizer: settings,
        ..AppConfig::default()
    };
    let json = serde_json::to_string(&config).expect("serialize config");
    let reloaded: AppConfig = serde_json::from_str(&json).expect("reload config");
    assert_eq!(reloaded.equalizer, settings);

    let old: AppConfig = serde_json::from_str(r#"{"library_paths":["/music"]}"#).unwrap();
    assert_eq!(old.equalizer, EqualizerSettings::default());

    for malformed in [
        r#""loud""#,
        r#"{"preset":"disco"}"#,
        r#"{"bands_db":[1.0,2.0]}"#,
        r#"{"enabled":"yes"}"#,
    ] {
        let json = format!(r#"{{"library_paths":["/music"],"equalizer":{malformed}}}"#);
        let config: AppConfig = serde_json::from_str(&json).expect("config still loads");
        assert_eq!(config.library_paths, ["/music"], "{malformed}");
        assert_eq!(
            config.equalizer,
            EqualizerSettings::default(),
            "{malformed}"
        );
    }

    let partial: AppConfig =
        serde_json::from_str(r#"{"equalizer":{"enabled":true,"preamp_db":99}}"#).unwrap();
    assert!(partial.equalizer.enabled);
    assert!((partial.equalizer.preamp_db - MAX_GAIN_DB).abs() < f64::EPSILON);
    assert_eq!(partial.equalizer.preset, Preset::Custom);
}

// ── Real pipelines ──────────────────────────────────────────────────────

/// Whether every element these tests use is installed. The first
/// equalizer is built here, once, so parallel tests never race the plugins'
/// first load and type registration.
fn plugins_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    let available = *AVAILABLE.get_or_init(|| {
        gst::init().is_ok()
            && EqualizerBin::new().is_ok()
            && [
                "audiotestsrc",
                "level",
                "fakesink",
                "wavenc",
                "wavparse",
                "playbin3",
            ]
            .iter()
            .all(|factory| gst::ElementFactory::find(factory).is_some())
    });
    if !available {
        eprintln!("skipping: GStreamer equalizer, level, or playback elements are not installed");
    }
    available
}

fn enabled() -> EqualizerSettings {
    EqualizerSettings {
        enabled: true,
        ..EqualizerSettings::default()
    }
}

fn tone_caps(rate: i32, channels: i32) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", rate)
        .field("channels", channels)
        .build()
}

/// `audiotestsrc ! capsfilter ! <equalizer> ! level ! fakesink`, playing a
/// `hz` tone.
fn tone_pipeline(equalizer: &EqualizerBin, amplitude: f64, hz: f64) -> gst::Pipeline {
    let source = gst::ElementFactory::make("audiotestsrc")
        .name("source")
        .property("freq", hz)
        .property("volume", amplitude)
        .property("num-buffers", 60)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property("caps", tone_caps(44_100, 2))
        .build()
        .unwrap();
    let level = gst::ElementFactory::make("level")
        .property("interval", 50_000_000_u64)
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    let pipeline = gst::Pipeline::new();
    let chain = [&source, &caps, equalizer.element(), &level, &sink];
    pipeline.add_many(chain).unwrap();
    gst::Element::link_many(chain).unwrap();
    pipeline
}

/// Loudest channel's value of a `level` message field, in dBFS.
fn loudest(structure: &gst::StructureRef, field: &str) -> f64 {
    structure
        .get::<glib::ValueArray>(field)
        .unwrap()
        .iter()
        .map(|value| value.get::<f64>().unwrap())
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Play the pipeline to the end and return each `level` message's
/// `(peak, rms)` in dBFS. Leaves the pipeline in `NULL`, as a track change does.
fn run(pipeline: &impl IsA<gst::Element>) -> Vec<(f64, f64)> {
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let mut levels = Vec::new();
    loop {
        let message = bus
            .timed_pop(gst::ClockTime::from_seconds(10))
            .expect("pipeline stalled");
        match message.view() {
            gst::MessageView::Element(element) => {
                if let Some(level) = element.structure().filter(|s| s.name() == "level") {
                    levels.push((loudest(level, "peak"), loudest(level, "rms")));
                }
            }
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(error) => panic!("pipeline error: {}", error.error()),
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(levels.len() > 4, "too few level readings: {levels:?}");
    levels
}

/// RMS once the filters have settled: the mean of every reading after the first two.
fn settled_rms(levels: &[(f64, f64)]) -> f64 {
    let settled = &levels[2..];
    settled.iter().map(|(_, rms)| rms).sum::<f64>()
        / f64::from(u32::try_from(settled.len()).unwrap())
}

fn peak(levels: &[(f64, f64)]) -> f64 {
    levels
        .iter()
        .map(|(peak, _)| *peak)
        .fold(f64::NEG_INFINITY, f64::max)
}

fn measure_at(settings: &EqualizerSettings, amplitude: f64, hz: f64) -> Vec<(f64, f64)> {
    let equalizer = EqualizerBin::new().unwrap();
    equalizer.apply(settings);
    run(&tone_pipeline(&equalizer, amplitude, hz))
}

fn measure(settings: &EqualizerSettings, amplitude: f64) -> Vec<(f64, f64)> {
    measure_at(settings, amplitude, TONE_HZ)
}

fn assert_near(actual: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: measured {actual:.2} dB, expected {expected:.2} ± {tolerance} dB"
    );
}

#[test]
fn preamp_and_band_gains_change_the_measured_level() {
    if !plugins_available() {
        return;
    }
    let rms = |settings: &EqualizerSettings| settled_rms(&measure(settings, 0.25));

    assert_near(
        rms(&EqualizerSettings::default()),
        TONE_RMS_DB,
        0.1,
        "disabled",
    );

    let attenuated = EqualizerSettings {
        preamp_db: -6.0,
        ..enabled()
    };
    assert_near(rms(&attenuated), TONE_RMS_DB - 6.0, 0.1, "preamp -6 dB");

    let mut boosted = enabled();
    boosted.bands_db[5] = 6.0;
    assert_near(rms(&boosted), TONE_RMS_DB + 6.0, 0.5, "1 kHz band +6 dB");

    let bypassed = EqualizerSettings {
        enabled: false,
        ..boosted
    };
    assert_near(rms(&bypassed), TONE_RMS_DB, 0.1, "disabled with gains set");
}

#[allow(clippy::float_cmp)] // the bands read back the exact values written
#[test]
fn the_bands_are_peak_filters_on_the_iso_centres() {
    if !plugins_available() {
        return;
    }
    let equalizer = EqualizerBin::new().unwrap();
    let element = equalizer.bin.by_name("bands").unwrap();
    assert_eq!(element.factory().unwrap().name(), "equalizer-nbands");
    assert_eq!(element.property::<u32>("num-bands"), 10);
    assert_eq!(equalizer.bands.len(), 10);
    for (index, (band, hz)) in equalizer.bands.iter().zip(BAND_CENTERS_HZ).enumerate() {
        assert_eq!(band.property::<f64>("freq"), f64::from(hz), "band {index}");
        assert_eq!(
            band.property::<f64>("bandwidth"),
            f64::from(band_width_hz(index)),
            "band {index}"
        );
        let kind = band.property_value("type");
        let (_, kind) = glib::EnumValue::from_value(&kind).expect("band type is an enum");
        assert_eq!(kind.nick(), "peak", "band {index}");
    }
}

#[test]
fn every_band_reaches_its_gain_at_its_own_centre() {
    if !plugins_available() {
        return;
    }
    for (index, hz) in BAND_CENTERS_HZ.iter().enumerate() {
        let hz = f64::from(*hz);
        let neutral = settled_rms(&measure_at(&enabled(), 0.25, hz));
        let mut boosted = enabled();
        boosted.bands_db[index] = 6.0;
        let lifted = settled_rms(&measure_at(&boosted, 0.25, hz));
        // A shelf would reach only half its gain here.
        assert_near(lifted - neutral, 6.0, 0.5, &format!("{hz} Hz band +6 dB"));
        boosted.bands_db[index] = MIN_GAIN_DB;
        let cut = settled_rms(&measure_at(&boosted, 0.25, hz));
        assert_near(
            cut - neutral,
            MIN_GAIN_DB,
            0.5,
            &format!("{hz} Hz band cut"),
        );
    }
}

#[test]
fn soft_clip_protection_keeps_peaks_at_or_below_full_scale() {
    if !plugins_available() {
        return;
    }
    let hot = EqualizerSettings {
        preamp_db: MAX_GAIN_DB,
        ..enabled()
    };
    let unprotected = peak(&measure(&hot, 0.9));
    assert!(unprotected > 10.0, "unprotected peak {unprotected:.2} dBFS");

    let protected = EqualizerSettings {
        clip_protection: ClipProtection::Soft,
        ..hot
    };
    let limited = peak(&measure(&protected, 0.9));
    assert!(
        limited <= 0.0,
        "limited peak {limited:.4} dBFS exceeds full scale"
    );
}

#[test]
fn settings_written_mid_stream_take_effect() {
    if !plugins_available() {
        return;
    }
    let equalizer = Arc::new(EqualizerBin::new().unwrap());
    let pipeline = tone_pipeline(&equalizer, 0.25, TONE_HZ);
    let source_pad = pipeline
        .by_name("source")
        .unwrap()
        .static_pad("src")
        .unwrap();
    let buffers = AtomicUsize::new(0);
    let writer = Arc::clone(&equalizer);
    source_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        if buffers.fetch_add(1, Ordering::SeqCst) == 30 {
            writer.apply(&EqualizerSettings {
                preamp_db: -12.0,
                ..enabled()
            });
        }
        gst::PadProbeReturn::Ok
    });

    let levels = run(&pipeline);
    assert_near(levels[2].1, TONE_RMS_DB, 0.1, "before the write");
    assert_near(
        levels[levels.len() - 1].1,
        TONE_RMS_DB - 12.0,
        0.1,
        "after the write",
    );
}

/// Write the test tone, at `rate` and `channels`, to a WAV file.
fn write_tone_file(path: &Path, rate: i32, channels: i32) {
    let source = gst::ElementFactory::make("audiotestsrc")
        .property("freq", TONE_HZ)
        .property("volume", 0.25)
        .property("num-buffers", 60)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property("caps", tone_caps(rate, channels))
        .build()
        .unwrap();
    let encoder = gst::ElementFactory::make("wavenc").build().unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path)
        .build()
        .unwrap();
    let pipeline = gst::Pipeline::new();
    let chain = [&source, &caps, &encoder, &sink];
    pipeline.add_many(chain).unwrap();
    gst::Element::link_many(chain).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let end = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(10),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    assert!(end.is_some_and(|message| message.type_() == gst::MessageType::Eos));
    pipeline.set_state(gst::State::Null).unwrap();
}

/// `playbin3` with the player's equalizer installed, playing into
/// `level ! fakesink`.
fn level_playbin() -> (gst::Element, PlayerEqualizer) {
    let sink =
        gst::parse::bin_from_description("level interval=50000000 ! fakesink sync=false", true)
            .unwrap();
    let playbin = gst::ElementFactory::make("playbin3")
        .property("audio-sink", &sink)
        .build()
        .unwrap();
    let equalizer = PlayerEqualizer::install(&playbin);
    assert!(equalizer.is_available());
    (playbin, equalizer)
}

#[test]
fn the_installed_bin_follows_rate_and_channel_changes_between_loads() {
    if !plugins_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (playbin, equalizer) = level_playbin();
    equalizer.apply(&EqualizerSettings {
        preamp_db: -6.0,
        ..enabled()
    });
    // Each load starts from NULL, as `Player` loads do.
    for (rate, channels) in [(44_100, 1), (96_000, 6), (48_000, 2)] {
        let file = dir.path().join(format!("{rate}-{channels}.wav"));
        write_tone_file(&file, rate, channels);
        playbin.set_property("uri", glib::filename_to_uri(&file, None).unwrap());
        let rms = settled_rms(&run(&playbin));
        assert_near(
            rms,
            TONE_RMS_DB - 6.0,
            0.1,
            &format!("{rate} Hz × {channels}"),
        );
    }
}

/// Make the band filter fail part-way through the stream, as a broken
/// element would: post an error and return a flow error upstream.
fn fail_bands_after(equalizer: &PlayerEqualizer, buffers: usize) {
    let bands = equalizer
        .bin
        .borrow()
        .as_ref()
        .unwrap()
        .bin
        .by_name("bands")
        .unwrap();
    let seen = AtomicUsize::new(0);
    bands
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if seen.fetch_add(1, Ordering::SeqCst) != buffers {
                return gst::PadProbeReturn::Ok;
            }
            let element = pad.parent_element().unwrap();
            gst::element_error!(element, gst::StreamError::Failed, ["injected failure"]);
            info.flow_res = Err(gst::FlowError::Error);
            gst::PadProbeReturn::Handled
        });
}

/// Play to the end, handling errors and prerolls as the player's bus watch
/// does. Panics on any error the equalizer does not recover from; returns
/// whether the stream was restarted and then resumed.
fn play_recovering(playbin: &gst::Element, equalizer: &PlayerEqualizer) -> (bool, bool) {
    playbin.set_state(gst::State::Playing).unwrap();
    let bus = playbin.bus().unwrap();
    let (mut resume_at, mut recovered, mut resumed) = (None, false, false);
    loop {
        let message = bus
            .timed_pop(gst::ClockTime::from_seconds(10))
            .expect("playback stalled");
        match message.view() {
            gst::MessageView::Error(error) => {
                assert!(!recovered, "error after recovery: {}", error.error());
                resume_at = equalizer.recover(&message, playbin);
                assert!(resume_at.is_some(), "{}", error.error());
                recovered = true;
            }
            gst::MessageView::AsyncDone(_) => {
                if let Some(position) = resume_at.take() {
                    let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT;
                    playbin.seek_simple(flags, position).unwrap();
                    resumed = true;
                }
            }
            gst::MessageView::Eos(_) => break,
            _ => {}
        }
    }
    playbin.set_state(gst::State::Null).unwrap();
    (recovered, resumed)
}

#[test]
fn an_error_inside_the_equalizer_drops_it_and_playback_continues() {
    if !plugins_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tone.wav");
    write_tone_file(&file, 44_100, 2);
    let (playbin, equalizer) = level_playbin();
    playbin.set_property("uri", glib::filename_to_uri(&file, None).unwrap());
    fail_bands_after(&equalizer, 20);

    let (recovered, resumed) = play_recovering(&playbin, &equalizer);
    assert!(recovered, "the injected failure never reached the bus");
    assert!(resumed, "the restarted stream never prerolled");
    assert!(!equalizer.is_available());
    assert!(playbin
        .property::<Option<gst::Element>>("audio-filter")
        .is_none());
}
