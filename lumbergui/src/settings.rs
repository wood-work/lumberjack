//! Settings that belong to the person, not to the project.
//!
//! A project says what the rig is and how it is plotted, and travels with the
//! rig — it is the same on whatever machine opens it. What theme somebody
//! likes and how big they need the text is the opposite: it follows the person
//! across every project they open, and has no business in a project folder.
//! So it lives beside the application rather than beside the data.

use iced::theme::Palette;
use iced::{color, Theme};
use std::sync::OnceLock;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// How many sizes the text has, and which one is ordinary.
///
/// Steps rather than a point size, for the same reason the history spans are
/// stepped: this is "make it bigger", not typography. Five is enough to be
/// useful at both ends without any of them being a shrug.
pub const FONT_STEPS: u8 = 5;
pub const DEFAULT_FONT_STEP: u8 = 3;

/// What each step does to the whole interface.
///
/// Scaling everything rather than only the text: iced's `scale_factor` takes
/// the layout with it, so rows, padding and icons grow together and nothing
/// collides. Centred on 1.0 at step 3, so the ordinary setting is genuinely
/// unscaled rather than merely nearby.
const SCALES: [f32; FONT_STEPS as usize] = [0.8, 0.9, 1.0, 1.15, 1.3];

/// The theme to fall back on.
const DEFAULT_THEME: &str = "Lumberjack";

/// The application's own theme.
///
/// Six colours, from which iced derives everything else: eight shades of the
/// background between `weakest` and `strongest`, and a weak, base and strong
/// of each accent with a text colour picked to read against it. What is set
/// here is therefore the whole of the decision - the rest follows.
///
/// Built once. `Theme::custom` generates that extended palette on every call,
/// and this is asked for while drawing.
fn lumberjack() -> Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();

    THEME
        .get_or_init(|| {
            Theme::custom(
                "Lumberjack".to_string(),
                Palette {
                    background: color!(0x2B2B2B),
                    text: color!(0xCFC8C8),
                    primary: color!(0x356F74),
                    success: color!(0x72CA77),
                    warning: color!(0xFFB16C),
                    danger: color!(0xF28585),
                },
            )
        })
        .clone()
}

/// Every theme on offer, ours first.
///
/// `Theme::ALL` is iced's built-in list and cannot know about a custom one, so
/// the two are joined here rather than anywhere a picker happens to be drawn.
pub fn themes() -> Vec<Theme> {
    let mut all = vec![lumberjack()];
    all.extend(Theme::ALL.iter().cloned());
    all
}

/// What the person has chosen, as it is written down.
///
/// The theme is stored by name rather than as a `Theme`, so the file does not
/// depend on the shape of an iced enum. A theme that is renamed, removed, or
/// simply misspelled by hand reads back as the default instead of failing to
/// parse and taking every other setting down with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    pub theme: String,
    pub font_step: u8,
    /// The last project opened, so it can be offered again.
    ///
    /// A convenience, not a record: it is offered only when the folder is
    /// still a project, and nothing goes wrong if it has been moved or
    /// deleted since. It lives here rather than in a project because it is
    /// about this person's habits, not about any one rig.
    pub last_project: Option<PathBuf>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_string(),
            font_step: DEFAULT_FONT_STEP,
            last_project: None,
        }
    }
}

impl UserSettings {
    /// What to multiply the whole interface by.
    ///
    /// Clamped rather than indexed blind: the file is editable by hand, and a
    /// `font_step` of 99 in it should give big text, not a panic on startup.
    pub fn scale_factor(&self) -> f32 {
        let step = self.font_step.clamp(1, FONT_STEPS);
        SCALES[usize::from(step) - 1]
    }

    /// The theme named, or the default if the name means nothing here.
    pub fn theme(&self) -> Theme {
        themes()
            .into_iter()
            .find(|theme| theme.to_string() == self.theme)
            .unwrap_or_else(lumberjack)
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.to_string();
    }

    pub fn set_last_project(&mut self, directory: &Path) {
        self.last_project = Some(directory.to_path_buf());
    }

    /// Read settings out of JSON.
    ///
    /// A leading byte order mark is dropped first. `serde_json` rejects one,
    /// and on Windows it is what Notepad and PowerShell's `Out-File` leave at
    /// the front of a UTF-8 file — so a file somebody edited by hand is a
    /// likely place to meet one. Nothing else is repaired: `#[serde(default)]`
    /// already covers a missing field, which is what every existing file looks
    /// like the moment this struct gains one.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
    }


    /// Errors rather than falling back, unlike reading.
    ///
    /// A default on the way *in* costs nothing; a default on the way *out*
    /// would write over a good file with an empty one. The two directions are
    /// not symmetrical, and the writing one has to be able to refuse.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Read the settings file, or the defaults if there isn't one.
    ///
    /// Split from `parse` so that everything with a decision in it can be
    /// tested without touching the real settings file — the part left here is
    /// only the read.
    ///
    /// Never fails, but does explain itself. A missing file is the ordinary
    /// first run and says nothing; a file that is there and will not parse is
    /// worth a line in the log, because quietly ignoring it looks exactly like
    /// the settings not working.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = settings_path() else {
            return (Self::default(), None);
        };

        let Ok(text) = fs::read_to_string(&path) else {
            return (Self::default(), None);
        };

        match Self::parse(&text) {
            Ok(settings) => (settings, None),
            Err(problem) => (
                Self::default(),
                Some(format!(
                    "{} could not be read ({}), so the usual settings are in use",
                    path.display(),
                    problem
                )),
            ),
        }
    }

    /// Write the settings file, creating the directory if it is not there.
    ///
    /// Errors are returned rather than swallowed: failing to save is worth
    /// telling somebody about, even though it is not worth stopping for.
    pub fn save(&self) -> io::Result<PathBuf> {
        let Some(path) = settings_path() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no configuration directory: neither APPDATA, XDG_CONFIG_HOME nor HOME is set",
            ));
        };

        let text = self.to_json().map_err(io::Error::other)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, text)?;
        Ok(path)
    }
}

/// Where the settings file lives on this machine.
///
/// Done with `env` rather than a crate: this is the whole of what `dirs` would
/// be pulled in for, and the two rules that matter are short. `APPDATA` is set
/// on Windows and not on Unix, which is what makes checking it first enough to
/// tell the platforms apart; `XDG_CONFIG_HOME` then `~/.config` is the Unix
/// convention. macOS gets `~/.config` too, which is not where a Mac app would
/// normally put this — worth revisiting if this ever ships there.
fn settings_path() -> Option<PathBuf> {
    Some(config_dir()?.join("settings.json"))
}

/// The folder this person's own files live in: their settings, and the log.
///
/// Shared rather than worked out twice, so the two cannot end up in different
/// places — and so somebody sending a settings file and a log is sending two
/// files from one folder.
pub(crate) fn config_dir() -> Option<PathBuf> {
    let directory = if let Some(appdata) = env::var_os("APPDATA") {
        PathBuf::from(appdata)
    } else if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(env::var_os("HOME")?).join(".config")
    };

    Some(directory.join("lumberjack"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ordinary_step_does_not_scale() {
        assert_eq!(UserSettings::default().font_step, DEFAULT_FONT_STEP);
        assert_eq!(UserSettings::default().scale_factor(), 1.0);
    }

    #[test]
    fn every_step_has_a_scale_and_they_only_grow() {
        let mut last = 0.0;
        for step in 1..=FONT_STEPS {
            let settings = UserSettings { font_step: step, ..Default::default() };
            let scale = settings.scale_factor();
            assert!(scale > last, "step {step} did not grow on {last}");
            last = scale;
        }
    }

    #[test]
    fn a_step_from_outside_the_range_is_clamped_not_panicked() {
        let small = UserSettings { font_step: 0, ..Default::default() };
        let large = UserSettings { font_step: 99, ..Default::default() };

        assert_eq!(small.scale_factor(), SCALES[0]);
        assert_eq!(large.scale_factor(), SCALES[SCALES.len() - 1]);
    }

    #[test]
    fn a_theme_survives_the_round_trip_by_name() {
        let mut settings = UserSettings::default();
        settings.set_theme(&Theme::Dracula);

        let written = settings.to_json().expect("settings should serialise");
        let read = UserSettings::parse(&written).expect("its own output should parse");
        assert_eq!(read.theme(), Theme::Dracula);
    }

    #[test]
    fn a_theme_that_no_longer_exists_falls_back() {
        let settings = UserSettings { theme: "Chartreuse".to_string(), ..Default::default() };
        assert_eq!(settings.theme(), lumberjack());
    }

    #[test]
    fn the_default_is_the_applications_own_theme() {
        assert_eq!(UserSettings::default().theme(), lumberjack());
        assert_eq!(lumberjack().to_string(), DEFAULT_THEME);
    }

    #[test]
    fn the_applications_theme_is_offered_alongside_the_built_in_ones() {
        let offered = themes();

        assert_eq!(offered.first(), Some(&lumberjack()));
        assert_eq!(offered.len(), Theme::ALL.len() + 1);
    }

    #[test]
    fn a_missing_field_takes_its_default() {
        let settings = UserSettings::parse(r#"{"theme": "Dracula"}"#).expect("valid json");

        assert_eq!(settings.theme(), Theme::Dracula);
        assert_eq!(settings.font_step, DEFAULT_FONT_STEP);
    }

    #[test]
    fn a_byte_order_mark_does_not_stop_it_parsing() {
        // What Notepad and PowerShell's `Out-File` leave at the front of a
        // file they have written as UTF-8.
        let settings = UserSettings::parse("\u{feff}{\"theme\": \"Dracula\"}")
            .expect("a byte order mark should not stop it");
        assert_eq!(settings.theme(), Theme::Dracula);
    }

    #[test]
    fn a_remembered_project_survives_the_round_trip() {
        let mut settings = UserSettings::default();
        settings.set_last_project(Path::new(r"C:\rigs\turbine"));

        let written = settings.to_json().expect("settings should serialise");
        let read = UserSettings::parse(&written).expect("its own output should parse");

        assert_eq!(read.last_project.as_deref(), Some(Path::new(r"C:\rigs\turbine")));
    }

    #[test]
    fn no_remembered_project_is_the_default() {
        assert_eq!(UserSettings::default().last_project, None);
    }

    #[test]
    fn nonsense_is_an_error_worth_reporting() {
        assert!(UserSettings::parse("not json at all").is_err());
    }
}
