//! Equalizer contract implementation (docs/equalizer.md, issue #49).
//!
//! This module owns the four bounded pieces of the equalizer feature:
//!
//! - [`EqSettings`] — the single typed state struct (enabled, preset,
//!   preamp, ten band gains, clip protection) with the contract's fixed
//!   bounds and fresh-install defaults.
//! - Persistence — the six-key `equalizer.cfg` grammar beside the volume
//!   file, with strict validation, boundary clamping, and an atomic-replace
//!   writer (temp file + fsync + rename + directory fsync).
//! - [`build_eq_bin`] — the local-pipeline GStreamer filter bin per the
//!   *Filter graph* section (`audioresample ! audioconvert ! capsfilter !
//!   volume ! equalizer-10bands [! rglimiter] ! audioconvert !
//!   audioresample ! capsfilter`).
//! - [`apply_band_transaction`] — the buffer-boundary property-write
//!   transaction (freeze notify → write → thaw notify) for bands and
//!   preamp.
//!
//! The capability matrix (which outputs can render equalizer DSP) is
//! reported honestly by each [`AudioOutput`](super::output::AudioOutput)
//! implementation through `supports_equalizer`; this module exposes the
//! shared matrix helper [`output_type_supports_equalizer`].
//!
//! Band centres, preset gain vectors, bounds, defaults, and diagnostics
//! wording are all fixed by the design document. Deviations are contract
//! changes and require a revision there first.

use std::path::PathBuf;

use gst::prelude::*;
use gstreamer as gst;

// ── Contract constants ──────────────────────────────────────────────────

/// Canonical ten band centre frequencies (Hz) of `equalizer-10bands`
/// (gst-plugins-good 1.28.5, verified with `gst-inspect-1.0`).
pub const BAND_CENTERS_HZ: [u32; 10] = [29, 59, 119, 237, 474, 947, 1889, 3770, 7523, 15011];

/// Minimum band/preamp gain in dB (contract bound).
pub const MIN_GAIN_DB: f64 = -24.0;

/// Maximum band/preamp gain in dB (contract bound).
pub const MAX_GAIN_DB: f64 = 12.0;

/// The only supported on-disk schema version.
const SCHEMA_VERSION: &str = "1";

/// Debounce window for coalescing slider-drag persistence writes.
pub const SAVE_DEBOUNCE_MS: u64 = 750;

// ── Types ───────────────────────────────────────────────────────────────

/// Named equalizer preset. `Custom` is write-only: it is never selectable
/// from the UI and is persisted only as a side-effect of a manual edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Preset {
    #[default]
    Flat,
    Pop,
    Rock,
    Jazz,
    Classical,
    Custom,
}

impl Preset {
    /// Persistence key (always English regardless of UI locale).
    pub fn key(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Pop => "pop",
            Self::Rock => "rock",
            Self::Jazz => "jazz",
            Self::Classical => "classical",
            Self::Custom => "custom",
        }
    }

    /// Parse a persistence key. Unknown/legacy values coerce to `Flat`
    /// per the validation rules (the band vector is not touched).
    pub fn from_key(key: &str) -> Self {
        match key {
            "pop" => Self::Pop,
            "rock" => Self::Rock,
            "jazz" => Self::Jazz,
            "classical" => Self::Classical,
            "custom" => Self::Custom,
            _ => Self::Flat,
        }
    }

    /// Menu position of `Custom` in the fixed six-entry preset combo.
    pub const CUSTOM_MENU_POSITION: usize = 5;

    /// Fixed band gain vector (dB) for this preset — the design appendix
    /// is the source of truth; any deviation is a contract change.
    pub fn band_gains_db(self) -> [f64; 10] {
        match self {
            //                                      29   59  119  237  474  947 1889 3770 7523 15011
            Self::Pop => [1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0],
            Self::Rock => [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            Self::Jazz => [2.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 2.0, 2.0, 1.0],
            Self::Classical => [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
            Self::Flat | Self::Custom => [0.0; 10],
        }
    }

    /// Recommended preamp (dB) written together with the band vector.
    pub fn recommended_preamp_db(self) -> f64 {
        match self {
            Self::Pop | Self::Classical => -2.0,
            Self::Rock | Self::Jazz => -1.0,
            Self::Flat | Self::Custom => 0.0,
        }
    }
}

/// Clip-protection policy. `Soft` inserts the fixed `rglimiter` element
/// (soft-knee compressor, −6 dBFS threshold, asymptotic 0 dBFS ceiling).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClipProtection {
    #[default]
    Off,
    Soft,
}

impl ClipProtection {
    pub fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Soft => "soft",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "soft" => Self::Soft,
            _ => Self::Off,
        }
    }
}

/// The complete equalizer state — one typed struct captured per write
/// transaction (contract: *Band and preamp mechanics*).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EqSettings {
    /// Global bypass. `false` means no equalizer bin exists in the
    /// pipeline at all; persisted settings stay on disk untouched.
    pub enabled: bool,
    /// Active preset. `Custom` after any manual band/preamp edit.
    pub preset: Preset,
    /// Global preamp in linear dB, `−24.0 … 0.0 … +12.0`, 0.5 dB steps.
    pub preamp_db: f64,
    /// Ten band gains in linear dB, index 0 = 29 Hz … index 9 = 15011 Hz.
    pub bands_db: [f64; 10],
    /// Clip-protection policy.
    pub clip_protection: ClipProtection,
}

impl EqSettings {
    /// Clamp a gain value into the contract range. Used on read; the UI
    /// must reject out-of-range input at the boundary instead.
    pub fn clamp_gain_db(value: f64) -> f64 {
        value.clamp(MIN_GAIN_DB, MAX_GAIN_DB)
    }

    /// Convert a preamp dB value to the `volume` element's linear factor
    /// (`factor = 10^(dB/20)`); `0.0 dB` maps to unity `1.0`.
    pub fn preamp_db_to_factor(preamp_db: f64) -> f64 {
        10.0_f64.powf(preamp_db / 20.0)
    }

    /// True when this state is byte-for-byte the fresh-install default.
    /// Persistence is suppressed entirely in this state.
    pub fn is_fresh_default(&self) -> bool {
        *self == Self::default()
    }

    /// The manual-edit side effect: any band or preamp change from a
    /// named preset moves the persisted name to `Custom`.
    pub fn mark_custom(&mut self) {
        self.preset = Preset::Custom;
    }
}

// ── Capability matrix ───────────────────────────────────────────────────

/// Capability-matrix row for an output type. Equalizer DSP runs in the
/// local pipeline only; AirPlay, Chromecast, and MPD receivers render
/// audio end-to-end and are `unsupported` under this contract.
///
/// Production reporting flows through the per-implementation
/// `AudioOutput::supports_equalizer` overrides; this shared helper
/// documents and tests the matrix itself.
#[allow(dead_code)]
pub fn output_type_supports_equalizer(output_type: super::output::OutputType) -> bool {
    matches!(output_type, super::output::OutputType::Local)
}

/// Localization key of the closed-form explanation shown by the settings
/// UI for an unsupported active output.
pub fn unsupported_explanation_key(output_type: super::output::OutputType) -> &'static str {
    match output_type {
        super::output::OutputType::Local => "equalizer.unsupported.local",
        super::output::OutputType::AirPlay => "equalizer.unsupported.airplay",
        super::output::OutputType::Chromecast => "equalizer.unsupported.chromecast",
        super::output::OutputType::Mpd => "equalizer.unsupported.mpd",
    }
}

// ── Persistence ─────────────────────────────────────────────────────────

/// Path to the equalizer state file: `<data_dir>/tributary/equalizer.cfg`,
/// beside the `volume` file. Owned exclusively by this module.
pub fn equalizer_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("equalizer.cfg"))
}

/// Append one `key="value"` line with the contract's escaping (`\"`,
/// `\\`) and no trailing newline skipping — every value is quoted.
fn push_quoted_line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str("=\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push_str("\"\n");
}

/// Render the canonical on-disk content: six keys in contract order, each
/// value double-quoted, floats with exactly one decimal place.
pub fn render_equalizer_file(settings: &EqSettings) -> String {
    let mut out = String::with_capacity(256);
    push_quoted_line(&mut out, "schema_version", SCHEMA_VERSION);
    push_quoted_line(&mut out, "enabled", &settings.enabled.to_string());
    push_quoted_line(&mut out, "preset", Preset::key(settings.preset));
    push_quoted_line(&mut out, "preamp_db", &format!("{:.1}", settings.preamp_db));
    for (index, gain) in settings.bands_db.iter().enumerate() {
        push_quoted_line(
            &mut out,
            &format!("band{index}_db"),
            &format!("{:.1}", gain),
        );
    }
    push_quoted_line(
        &mut out,
        "clip_protect",
        ClipProtection::key(settings.clip_protection),
    );
    out
}

/// What happened when the on-disk state was read.
#[derive(Debug)]
pub enum EqLoadOutcome {
    /// The file was valid (coercions and clamps may still have been
    /// applied per the validation rules).
    Loaded(EqSettings),
    /// The file was malformed or carried an unsupported schema version.
    /// Defaults are returned and have been re-written to disk via the
    /// same atomic-replace protocol.
    ReplacedWithDefaults {
        settings: EqSettings,
        diagnostic: EqFileDiagnostic,
    },
}

/// Bounded diagnostic for a replaced malformed file: file path, byte
/// count, and the offending key only — never the file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqFileDiagnostic {
    pub path: String,
    pub byte_count: u64,
    pub bad_key: String,
}

/// Read `equalizer.cfg`, validate, clamp, and coerce per the contract.
pub fn load_equalizer_settings_from_disk() -> EqLoadOutcome {
    let Some(path) = equalizer_path() else {
        return EqLoadOutcome::Loaded(EqSettings::default());
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // Missing file is the fresh-install shape: defaults, no write.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EqLoadOutcome::Loaded(EqSettings::default());
        }
        Err(_) => {
            // Unreadable (permissions, I/O): treat as malformed — the
            // parser cannot trust state it cannot see.
            return replace_with_defaults(&path, 0, "(unreadable)");
        }
    };
    match parse_equalizer_file(&bytes) {
        Ok(settings) => EqLoadOutcome::Loaded(settings),
        Err(bad_key) => replace_with_defaults(&path, bytes.len() as u64, &bad_key),
    }
}

fn replace_with_defaults(path: &std::path::Path, byte_count: u64, bad_key: &str) -> EqLoadOutcome {
    let settings = EqSettings::default();
    let diagnostic = EqFileDiagnostic {
        path: path.display().to_string(),
        byte_count,
        bad_key: bad_key.to_string(),
    };
    let _ = write_equalizer_file_atomic(path, &render_equalizer_file(&settings));
    EqLoadOutcome::ReplacedWithDefaults {
        settings,
        diagnostic,
    }
}

/// Parse the strict `key="value"` grammar. Returns `Err(bad_key)` for a
/// malformed line, a bad schema version, or an unparseable mandatory key.
fn parse_equalizer_file(bytes: &[u8]) -> Result<EqSettings, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "schema_version".to_string())?;
    let mut schema_version: Option<String> = None;
    let mut enabled: Option<bool> = None;
    let mut preset: Option<Preset> = None;
    let mut preamp_db: Option<f64> = None;
    let mut bands_db = [0.0_f64; 10];
    let mut clip_protection = ClipProtection::Off;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = parse_line(line).ok_or_else(|| bad_key_of(line))?;
        match key.as_str() {
            "schema_version" => schema_version = Some(value),
            "enabled" => {
                enabled = Some(match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => false,
                });
            }
            "preset" => preset = Some(Preset::from_key(&value)),
            "preamp_db" => {
                preamp_db = Some(parse_gain(&value).ok_or_else(|| "preamp_db".to_string())?);
            }
            "clip_protect" => clip_protection = ClipProtection::from_key(&value),
            _ => {
                if let Some(band) = band_index(&key) {
                    bands_db[band] = parse_gain(&value).ok_or_else(|| format!("band{band}_db"))?;
                }
                // Unknown keys are ignored so a future minor schema can
                // add keys without discarding user state.
            }
        }
    }

    let schema_version = schema_version.ok_or_else(|| "schema_version".to_string())?;
    if schema_version != SCHEMA_VERSION {
        return Err("schema_version".to_string());
    }

    let mut settings = EqSettings {
        enabled: enabled.unwrap_or(false),
        preset: preset.unwrap_or(Preset::Flat),
        preamp_db: preamp_db.unwrap_or(0.0),
        bands_db,
        clip_protection,
    };
    settings.preamp_db = EqSettings::clamp_gain_db(settings.preamp_db);
    for gain in &mut settings.bands_db {
        *gain = EqSettings::clamp_gain_db(*gain);
    }
    Ok(settings)
}

fn bad_key_of(line: &str) -> String {
    line.split('=')
        .next()
        .unwrap_or("(unknown)")
        .trim()
        .to_string()
}

fn band_index(key: &str) -> Option<usize> {
    let rest = key.strip_prefix("band")?;
    let index = rest.strip_suffix("_db")?;
    index.parse::<usize>().ok().filter(|i| *i < 10)
}

/// Parse one gain value: finite float, clamped by the caller.
fn parse_gain(value: &str) -> Option<f64> {
    let gain = value.trim().parse::<f64>().ok()?;
    gain.is_finite().then_some(gain)
}

/// Parse a single `key="value"` line with `\"` / `\\` escapes. A bare
/// (unquoted) value or an unknown escape is malformed.
fn parse_line(line: &str) -> Option<(String, String)> {
    let (key, rest) = line.split_once('=')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let value = rest.strip_prefix('"')?.strip_suffix('"')?;
    if rest.len() < 2 {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                _ => return None,
            },
            '"' => return None,
            '\n' | '\r' => return None,
            _ => out.push(ch),
        }
    }
    Some((key.to_string(), out))
}

/// Persist settings with the atomic-replace protocol: temp file
/// (`O_EXCL`, single write, fsync), `rename(2)`, then directory fsync.
/// A concurrent or failed writer never leaves a partial file visible.
pub fn save_equalizer_settings_to_disk(settings: &EqSettings) -> bool {
    let Some(path) = equalizer_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_equalizer_file_atomic(&path, &render_equalizer_file(settings)).is_ok()
}

fn write_equalizer_file_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;

    let temp_path = temp_sibling(path);
    let write_result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, path)?;
        // The rename alone is not durable until the directory entry is.
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

fn temp_sibling(path: &std::path::Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

// ── GStreamer filter bin ────────────────────────────────────────────────

/// Failure reasons for bin construction. All are recoverable: the caller
/// falls back to the existing passthrough layout and keeps going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqBinBuildError {
    /// A required GStreamer element (plugin) is not installed.
    ElementUnavailable(&'static str),
    /// An element could not be added or linked inside the bin.
    ConstructionFailed,
}

impl std::fmt::Display for EqBinBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ElementUnavailable(name) => write!(f, "GStreamer element unavailable: {name}"),
            Self::ConstructionFailed => write!(f, "equalizer bin construction failed"),
        }
    }
}

/// Handles into one installed equalizer bin. Retained by the local
/// player for the whole time the bin is linked into `playbin3`.
pub struct EqChain {
    /// The complete filter bin installed at `playbin3.audio-filter`.
    pub bin: gst::Bin,
    /// `volume` preamp stage (`eq-preamp`).
    preamp: gst::Element,
    /// `equalizer-10bands` stage (`eq`).
    eq: gst::Element,
    /// Post-EQ `audioconvert` — relink target for limiter surgery.
    post_convert: gst::Element,
    /// `rglimiter` stage (`clipper`), present iff clip protection is on.
    clipper: Option<gst::Element>,
}

fn make_element(factory: &'static str, name: &str) -> Result<gst::Element, EqBinBuildError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|_| EqBinBuildError::ElementUnavailable(factory))
}

/// Return one limiter element to NULL and remove it from the bin.
/// `gst_bin_remove` requires a NULL-state child.
fn drop_limiter_from_bin(bin: &gst::Bin, clipper: &gst::Element) {
    let _ = clipper.set_state(gst::State::Null);
    let _ = bin.remove(clipper);
}

impl EqChain {
    /// Build the filter bin per the *Filter graph* section for `settings`.
    ///
    /// The pre-EQ `capsfilter` pins
    /// `audio/x-raw,format=F32LE,channels=2,layout=interleaved`; the sample
    /// rate is deliberately left negotiable so `audioresample` follows the
    /// rate `playbin3` negotiates with the decoder instead of pinning a
    /// rate we cannot know at construction time. The post-EQ `capsfilter`
    /// is created with its caps unset for the same reason: the surrounding
    /// `audioconvert`/`audioresample` adapt to whatever the audio sink
    /// negotiates. If the upstream decoder cannot deliver the pinned caps,
    /// negotiation fails at the state transition, `playbin3` posts the
    /// error to the bus, and the caller rolls the bin back to passthrough.
    pub fn build(settings: &EqSettings) -> Result<Self, EqBinBuildError> {
        let bin = gst::Bin::with_name("eq-bin");

        let pre_resample = make_element("audioresample", "eq-pre-resample")?;
        let pre_convert = make_element("audioconvert", "eq-pre-convert")?;
        let format_pin = make_element("capsfilter", "eq-format-pin")?;
        let preamp = make_element("volume", "eq-preamp")?;
        let eq = make_element("equalizer-10bands", "eq")?;
        let post_convert = make_element("audioconvert", "eq-post-convert")?;
        let post_resample = make_element("audioresample", "eq-post-resample")?;
        let sink_pin = make_element("capsfilter", "eq-sink-pin")?;

        format_pin.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("channels", 2)
                .field("layout", "interleaved")
                .build(),
        );

        let elements: Vec<gst::Element> = if settings.clip_protection == ClipProtection::Soft {
            let clipper = make_element("rglimiter", "clipper")?;
            clipper.set_property("enabled", true);
            vec![
                pre_resample,
                pre_convert,
                format_pin,
                preamp.clone(),
                eq.clone(),
                clipper,
                post_convert.clone(),
                post_resample,
                sink_pin,
            ]
        } else {
            vec![
                pre_resample,
                pre_convert,
                format_pin,
                preamp.clone(),
                eq.clone(),
                post_convert.clone(),
                post_resample,
                sink_pin,
            ]
        };

        let rollback = |elements: &[gst::Element]| {
            for element in elements {
                let _ = bin.remove(element);
            }
        };

        if bin.add_many(&elements).is_err() {
            rollback(&elements);
            return Err(EqBinBuildError::ConstructionFailed);
        }
        if gst::Element::link_many(&elements).is_err() {
            rollback(&elements);
            return Err(EqBinBuildError::ConstructionFailed);
        }

        // Ghost pads carry the decoder-side and sink-side links.
        let first_sink = elements[0]
            .static_pad("sink")
            .ok_or(EqBinBuildError::ConstructionFailed)?;
        let last_src = elements
            .last()
            .and_then(|element| element.static_pad("src"))
            .ok_or(EqBinBuildError::ConstructionFailed)?;
        let sink_ghost = gst::GhostPad::builder_with_target(&first_sink)
            .map_err(|_| EqBinBuildError::ConstructionFailed)?
            .name("audio-filter-sink")
            .build();
        let src_ghost = gst::GhostPad::builder_with_target(&last_src)
            .map_err(|_| EqBinBuildError::ConstructionFailed)?
            .name("audio-filter-src")
            .build();
        if bin.add_pad(&sink_ghost).is_err() || bin.add_pad(&src_ghost).is_err() {
            rollback(&elements);
            return Err(EqBinBuildError::ConstructionFailed);
        }

        let clipper = elements
            .iter()
            .find(|element| element.name() == "clipper")
            .cloned();

        let chain = Self {
            bin,
            preamp,
            eq,
            post_convert,
            clipper,
        };
        chain.apply_band_transaction(settings);
        Ok(chain)
    }

    /// Buffer-boundary property-write transaction: capture the full
    /// `EqSettings` into one typed write, wrap the ten band writes and
    /// the preamp write in a notification freeze (RAII guard thawing on
    /// drop) so the bus sees exactly one `properties-changed` per
    /// element, not eleven.
    pub fn apply_band_transaction(&self, settings: &EqSettings) {
        {
            // The freeze guard thaws notifications when dropped.
            let _frozen = self.preamp.freeze_notify();
            self.preamp.set_property(
                "volume",
                EqSettings::preamp_db_to_factor(settings.preamp_db),
            );
        }
        {
            let _frozen = self.eq.freeze_notify();
            for (index, gain) in settings.bands_db.iter().enumerate() {
                // `equalizer-10bands` band properties are gdouble.
                self.eq.set_property(&format!("band{index}"), *gain);
            }
        }
    }

    /// Insert or remove the `rglimiter` element inside the installed bin
    /// (clip-protection toggle). The caller owns the pause/resume seam.
    /// Returns `false` when the surgery failed and the chain degraded to
    /// the no-limiter layout (recoverable per the contract).
    pub fn set_clip_protection(&mut self, soft: ClipProtection) -> bool {
        match (soft, self.clipper.take()) {
            (ClipProtection::Soft, None) => {
                let Ok(clipper) = make_element("rglimiter", "clipper") else {
                    return false;
                };
                clipper.set_property("enabled", true);
                if self.bin.add(&clipper).is_err() {
                    return false;
                }
                // The EQ stage already feeds the post-convert stage
                // directly; break that link to make room for the limiter.
                let was_linked = self
                    .eq
                    .static_pad("src")
                    .map(|src| src.peer().is_some())
                    .unwrap_or(false);
                if was_linked {
                    // `Element::unlink` returns `()`.
                    self.eq.unlink(&self.post_convert);
                }
                if self.eq.link(&clipper).is_ok() && clipper.link(&self.post_convert).is_ok() {
                    self.clipper = Some(clipper);
                    true
                } else {
                    // Degrade to the no-limiter layout: restore the
                    // direct eq → post-convert link.
                    drop_limiter_from_bin(&self.bin, &clipper);
                    let _ = self.eq.link(&self.post_convert);
                    false
                }
            }
            (ClipProtection::Off, Some(clipper)) => {
                self.eq.unlink(&clipper);
                drop_limiter_from_bin(&self.bin, &clipper);
                self.eq.link(&self.post_convert).is_ok()
            }
            (ClipProtection::Off, None) | (ClipProtection::Soft, Some(_)) => true,
        }
    }

    /// True when the `rglimiter` element is currently inside the bin.
    #[allow(dead_code)] // inspection helper; exercised by the contract tests
    pub fn clip_protection_installed(&self) -> bool {
        self.clipper.is_some()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests {
    use super::*;

    // ── Defaults ────────────────────────────────────────────────────

    #[test]
    fn fresh_install_default_matches_the_contract() {
        let defaults = EqSettings::default();
        assert!(!defaults.enabled);
        assert_eq!(defaults.preset, Preset::Flat);
        assert_eq!(defaults.preamp_db, 0.0);
        assert!(defaults.bands_db.iter().all(|gain| *gain == 0.0));
        assert_eq!(defaults.clip_protection, ClipProtection::Off);
        assert!(defaults.is_fresh_default());
    }

    #[test]
    fn band_centers_are_the_canonical_ten() {
        assert_eq!(
            BAND_CENTERS_HZ,
            [29, 59, 119, 237, 474, 947, 1889, 3770, 7523, 15011]
        );
    }

    // ── Preset appendix vectors ─────────────────────────────────────

    #[test]
    fn preset_vectors_match_the_design_appendix() {
        assert_eq!(Preset::Flat.band_gains_db(), [0.0; 10]);
        assert_eq!(Preset::Flat.recommended_preamp_db(), 0.0);

        assert_eq!(
            Preset::Pop.band_gains_db(),
            [1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0]
        );
        assert_eq!(Preset::Pop.recommended_preamp_db(), -2.0);

        assert_eq!(
            Preset::Rock.band_gains_db(),
            [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0]
        );
        assert_eq!(Preset::Rock.recommended_preamp_db(), -1.0);

        assert_eq!(
            Preset::Jazz.band_gains_db(),
            [2.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 2.0, 2.0, 1.0]
        );
        assert_eq!(Preset::Jazz.recommended_preamp_db(), -1.0);

        assert_eq!(
            Preset::Classical.band_gains_db(),
            [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]
        );
        assert_eq!(Preset::Classical.recommended_preamp_db(), -2.0);
    }

    // ── Render grammar ──────────────────────────────────────────────

    #[test]
    fn rendered_default_file_matches_the_contract_block() {
        let rendered = render_equalizer_file(&EqSettings::default());
        let expected = "\
schema_version=\"1\"
enabled=\"false\"
preset=\"flat\"
preamp_db=\"0.0\"
band0_db=\"0.0\"
band1_db=\"0.0\"
band2_db=\"0.0\"
band3_db=\"0.0\"
band4_db=\"0.0\"
band5_db=\"0.0\"
band6_db=\"0.0\"
band7_db=\"0.0\"
band8_db=\"0.0\"
band9_db=\"0.0\"
clip_protect=\"off\"
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn rendered_values_carry_one_decimal_and_canonical_order() {
        let settings = EqSettings {
            enabled: true,
            preset: Preset::Pop,
            preamp_db: -2.0,
            bands_db: [1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 12.0],
            clip_protection: ClipProtection::Soft,
        };
        let rendered = render_equalizer_file(&settings);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 15);
        assert_eq!(lines[0], "schema_version=\"1\"");
        assert_eq!(lines[1], "enabled=\"true\"");
        assert_eq!(lines[2], "preset=\"pop\"");
        assert_eq!(lines[3], "preamp_db=\"-2.0\"");
        assert_eq!(lines[4], "band0_db=\"1.0\"");
        assert_eq!(lines[13], "band9_db=\"12.0\"");
        assert_eq!(lines[14], "clip_protect=\"soft\"");
    }

    #[test]
    fn values_are_escaped_like_contract_values() {
        let mut out = String::new();
        push_quoted_line(&mut out, "preset", "sa\"fe\\path");
        assert_eq!(out, "preset=\"sa\\\"fe\\\\path\"\n");
    }

    // ── Parse: round-trip, coercions, clamps ────────────────────────

    #[test]
    fn parse_round_trips_a_written_file() {
        let settings = EqSettings {
            enabled: true,
            preset: Preset::Rock,
            preamp_db: -1.0,
            bands_db: [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            clip_protection: ClipProtection::Soft,
        };
        let parsed = parse_equalizer_file(render_equalizer_file(&settings).as_bytes())
            .expect("round trip parse");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn parser_is_order_insensitive_but_strict_about_quoting() {
        let reordered = "\
clip_protect=\"soft\"
band9_db=\"+12.0\"
schema_version=\"1\"
preamp_db=\"-24.0\"
preset=\"jazz\"
band0_db=\"-24.0\"
enabled=\"true\"
";
        let parsed = parse_equalizer_file(reordered.as_bytes()).expect("reordered parse");
        assert!(parsed.enabled);
        assert_eq!(parsed.preset, Preset::Jazz);
        assert_eq!(parsed.preamp_db, -24.0);
        assert_eq!(parsed.bands_db[9], 12.0);
        assert_eq!(parsed.clip_protection, ClipProtection::Soft);

        for malformed in ["preset=flat\n", "preset=flat\r\n", "preset=\"\n"] {
            assert!(
                parse_equalizer_file(malformed.as_bytes()).is_err(),
                "unquoted or empty value must be malformed: {malformed:?}"
            );
        }
    }

    #[test]
    fn out_of_range_gains_clamp_to_the_boundary() {
        let mut content = render_equalizer_file(&EqSettings::default());
        content = content.replace("preamp_db=\"0.0\"", "preamp_db=\"-99.0\"");
        content = content.replace("band0_db=\"0.0\"", "band0_db=\"+99.0\"");
        content = content.replace("band9_db=\"0.0\"", "band9_db=\"13.5\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("clamped parse");
        assert_eq!(parsed.preamp_db, MIN_GAIN_DB);
        assert_eq!(parsed.bands_db[0], MAX_GAIN_DB);
        assert_eq!(parsed.bands_db[9], MAX_GAIN_DB);
        // Other bands remain valid.
        assert_eq!(parsed.bands_db[5], 0.0);
    }

    #[test]
    fn unknown_preset_coerces_to_flat_and_keeps_the_band_vector() {
        let content = render_equalizer_file(&EqSettings {
            preset: Preset::Rock,
            bands_db: [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            ..EqSettings::default()
        })
        .replace("preset=\"rock\"", "preset=\"loudness2001\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("coerced parse");
        assert_eq!(parsed.preset, Preset::Flat);
        assert_eq!(parsed.bands_db[0], 3.0);
    }

    #[test]
    fn bad_boolean_and_clip_values_fall_back_to_off_states() {
        let content = render_equalizer_file(&EqSettings {
            enabled: true,
            clip_protection: ClipProtection::Soft,
            ..EqSettings::default()
        })
        .replace("enabled=\"true\"", "enabled=\"maybe\"")
        .replace("clip_protect=\"soft\"", "clip_protect=\"turbo\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("fallback parse");
        assert!(!parsed.enabled);
        assert_eq!(parsed.clip_protection, ClipProtection::Off);
    }

    #[test]
    fn malformed_line_reports_the_bad_key() {
        let content = render_equalizer_file(&EqSettings::default())
            .replace("band3_db=\"0.0\"", "band3_db=\"0.0\" trailing");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("malformed");
        assert_eq!(error, "band3_db");
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let content = render_equalizer_file(&EqSettings::default())
            .replace("schema_version=\"1\"", "schema_version=\"2\"");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("schema");
        assert_eq!(error, "schema_version");
    }

    #[test]
    fn missing_enabled_key_defaults_to_disabled() {
        // A missing optional key is tolerated with its default; only a
        // malformed line or a bad schema version replaces the file.
        let content = render_equalizer_file(&EqSettings {
            enabled: true,
            ..EqSettings::default()
        })
        .lines()
        .filter(|line| !line.starts_with("enabled="))
        .collect::<Vec<_>>()
        .join("\n");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("missing enabled tolerated");
        assert!(!parsed.enabled);
    }

    #[test]
    fn missing_schema_version_is_rejected() {
        let content = render_equalizer_file(&EqSettings::default())
            .lines()
            .filter(|line| !line.starts_with("schema_version="))
            .collect::<Vec<_>>()
            .join("\n");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("missing schema");
        assert_eq!(error, "schema_version");
    }

    #[test]
    fn non_finite_gain_is_malformed() {
        let content = render_equalizer_file(&EqSettings::default())
            .replace("band0_db=\"0.0\"", "band0_db=\"nan\"");
        assert!(parse_equalizer_file(content.as_bytes()).is_err());
    }

    // ── Preamp conversion ───────────────────────────────────────────

    #[test]
    fn preamp_factor_conversion_matches_the_contract_range() {
        assert!((EqSettings::preamp_db_to_factor(0.0) - 1.0).abs() < 1e-12);
        assert!((EqSettings::preamp_db_to_factor(-24.0) - 0.0631).abs() < 1e-4);
        assert!((EqSettings::preamp_db_to_factor(12.0) - 3.9811).abs() < 1e-4);
        // The whole contract range stays inside the volume element's 0..10.
        for step in -48..=24 {
            let db = step as f64 / 2.0;
            let factor = EqSettings::preamp_db_to_factor(db);
            assert!(
                (0.0..=10.0).contains(&factor),
                "{db} dB out of element range"
            );
        }
    }

    // ── Capability matrix ───────────────────────────────────────────

    #[test]
    fn only_the_local_output_claims_equalizer_support() {
        use super::super::output::OutputType;
        assert!(output_type_supports_equalizer(OutputType::Local));
        assert!(!output_type_supports_equalizer(OutputType::AirPlay));
        assert!(!output_type_supports_equalizer(OutputType::Chromecast));
        assert!(!output_type_supports_equalizer(OutputType::Mpd));
    }

    #[test]
    fn unsupported_explanations_have_a_dedicated_locale_key() {
        use super::super::output::OutputType;
        let keys = [
            unsupported_explanation_key(OutputType::Local),
            unsupported_explanation_key(OutputType::AirPlay),
            unsupported_explanation_key(OutputType::Chromecast),
            unsupported_explanation_key(OutputType::Mpd),
        ];
        assert!(keys.iter().all(|key| key.starts_with("equalizer.")));
    }

    // ── GStreamer bin structure ─────────────────────────────────────

    fn bin_requires_plugins() -> bool {
        gst::init().is_ok()
            && gst::ElementFactory::make("equalizer-10bands")
                .build()
                .is_ok()
            && gst::ElementFactory::make("rglimiter").build().is_ok()
    }

    #[test]
    fn eq_bin_layout_matches_the_filter_graph_with_limiter() {
        if !bin_requires_plugins() {
            // Minimal development hosts may omit gst-plugins-good. Packaged
            // builds require it, and CI's package jobs exercise that contract.
            return;
        }
        let settings = EqSettings {
            enabled: true,
            preset: Preset::Pop,
            preamp_db: -2.0,
            bands_db: Preset::Pop.band_gains_db(),
            clip_protection: ClipProtection::Soft,
        };
        let chain = EqChain::build(&settings).expect("eq-bin builds");
        assert_eq!(chain.bin.name(), "eq-bin");
        assert!(chain.bin.static_pad("audio-filter-sink").is_some());
        assert!(chain.bin.static_pad("audio-filter-src").is_some());
        assert!(chain.clip_protection_installed());

        // Pre-EQ capsfilter pins F32LE stereo interleaved.
        let format_pin = chain
            .bin
            .by_name("eq-format-pin")
            .expect("pre-EQ capsfilter present");
        let caps = format_pin.property::<gst::Caps>("caps");
        let structure = caps.structure(0).expect("caps structure");
        assert_eq!(structure.name().as_str(), "audio/x-raw");
        assert_eq!(structure.get::<String>("format").as_deref(), Ok("F32LE"));
        assert_eq!(structure.get::<i32>("channels"), Ok(2));
        assert_eq!(
            structure.get::<String>("layout").as_deref(),
            Ok("interleaved")
        );

        // Preset transaction reached the elements.
        let preamp = chain.bin.by_name("eq-preamp").expect("preamp present");
        let expected_factor = EqSettings::preamp_db_to_factor(-2.0);
        // The volume element quantizes its gain to f32 internally even
        // though the property is declared gdouble, so compare at f32
        // precision.
        assert!((preamp.property::<f64>("volume") - expected_factor).abs() < 1e-6);
        let eq = chain.bin.by_name("eq").expect("equalizer present");
        assert!((eq.property::<f64>("band0") - 1.0).abs() < 1e-9);
        assert!((eq.property::<f64>("band2") - 3.0).abs() < 1e-9);
        assert!((eq.property::<f64>("band5") - (-1.0)).abs() < 1e-9);

        // Chain order: eq → clipper → post-convert.
        let clipper = chain.bin.by_name("clipper").expect("limiter present");
        assert!(clipper.property::<bool>("enabled"));
        let eq_src_peer_parent = eq
            .static_pad("src")
            .unwrap()
            .peer()
            .expect("eq linked")
            .parent()
            .expect("peer parented");
        assert_eq!(eq_src_peer_parent.name(), clipper.name());
        let clipper_src_peer_parent = clipper
            .static_pad("src")
            .unwrap()
            .peer()
            .expect("clipper linked")
            .parent()
            .expect("peer parented");
        assert_eq!(
            clipper_src_peer_parent.name(),
            chain.bin.by_name("eq-post-convert").unwrap().name()
        );
    }

    #[test]
    fn eq_bin_omits_the_limiter_when_clip_protection_is_off() {
        if !bin_requires_plugins() {
            return;
        }
        let settings = EqSettings {
            enabled: true,
            ..EqSettings::default()
        };
        let chain = EqChain::build(&settings).expect("eq-bin builds");
        assert!(!chain.clip_protection_installed());
        assert!(chain.bin.by_name("clipper").is_none());
        // eq links directly to the post-convert stage.
        let eq = chain.bin.by_name("eq").unwrap();
        let eq_src_peer_parent = eq
            .static_pad("src")
            .unwrap()
            .peer()
            .expect("eq linked")
            .parent()
            .expect("peer parented");
        assert_eq!(
            eq_src_peer_parent.name(),
            chain.bin.by_name("eq-post-convert").unwrap().name()
        );
    }

    #[test]
    fn limiter_surgery_inserts_and_removes_inside_the_installed_bin() {
        if !bin_requires_plugins() {
            return;
        }
        let mut chain = EqChain::build(&EqSettings {
            enabled: true,
            ..EqSettings::default()
        })
        .expect("eq-bin builds");

        // Off → Soft: insert.
        assert!(chain.set_clip_protection(ClipProtection::Soft));
        assert!(chain.clip_protection_installed());
        let clipper = chain.bin.by_name("clipper").expect("inserted limiter");
        assert!(clipper.property::<bool>("enabled"));
        let eq = chain.bin.by_name("eq").unwrap();
        let eq_src_peer_parent = eq
            .static_pad("src")
            .unwrap()
            .peer()
            .expect("eq linked")
            .parent()
            .expect("peer parented");
        assert_eq!(eq_src_peer_parent.name(), clipper.name());

        // Soft → Off: remove.
        assert!(chain.set_clip_protection(ClipProtection::Off));
        assert!(!chain.clip_protection_installed());
        assert!(chain.bin.by_name("clipper").is_none());
        let eq_src_peer_parent = eq
            .static_pad("src")
            .unwrap()
            .peer()
            .expect("eq relinked")
            .parent()
            .expect("peer parented");
        assert_eq!(
            eq_src_peer_parent.name(),
            chain.bin.by_name("eq-post-convert").unwrap().name()
        );
    }

    #[test]
    fn band_transaction_updates_a_live_chain_in_one_write_set() {
        if !bin_requires_plugins() {
            return;
        }
        let chain = EqChain::build(&EqSettings {
            enabled: true,
            ..EqSettings::default()
        })
        .expect("eq-bin builds");

        let next = EqSettings {
            enabled: true,
            preset: Preset::Custom,
            preamp_db: 12.0,
            bands_db: [-24.0, -12.0, -6.0, -0.5, 0.0, 0.5, 6.0, 12.0, 3.5, 1.5],
            clip_protection: ClipProtection::Off,
        };
        chain.apply_band_transaction(&next);

        let preamp = chain.bin.by_name("eq-preamp").unwrap();
        assert!(
            (preamp.property::<f64>("volume") - EqSettings::preamp_db_to_factor(12.0)).abs() < 1e-6
        );
        let eq = chain.bin.by_name("eq").unwrap();
        for (index, expected) in next.bands_db.iter().enumerate() {
            let written: f64 = eq.property(&format!("band{index}"));
            assert!((written - *expected).abs() < 1e-9, "band{index}");
        }
    }

    // ── Atomic writer ───────────────────────────────────────────────

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_sibling() {
        let base = std::env::temp_dir().join(format!("tributary-eq-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("temp dir");
        let path = base.join("equalizer.cfg");

        assert!(write_equalizer_file_atomic(&path, "first=\"1\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first=\"1\"\n");
        assert!(!temp_sibling(&path).exists());

        assert!(write_equalizer_file_atomic(&path, "second=\"2\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second=\"2\"\n");
        assert!(!temp_sibling(&path).exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn temp_sibling_sits_beside_the_destination() {
        let path = std::path::Path::new("/tmp/tributary/equalizer.cfg");
        assert_eq!(
            temp_sibling(path),
            std::path::Path::new("/tmp/tributary/equalizer.cfg.tmp")
        );
    }
}
