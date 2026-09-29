mod markdown;

use std::{
    cell::RefCell,
    env, fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use serde::{Deserialize, Serialize};
use slint::{ModelRc, VecModel};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let Some(path) = env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: lantai <file.md>");
        std::process::exit(2);
    };
    let path = fs::canonicalize(&path).unwrap_or(path);

    let ui = AppWindow::new()?;
    let settings = Settings::load();
    let viewer = Rc::new(RefCell::new(Viewer::new(path, settings)));

    {
        let viewer = viewer.borrow();
        viewer.apply_to_ui(&ui);
        viewer.update_blocks(&ui);
        ui.set_doc_title(viewer.title().into());
    }

    let weak = ui.as_weak();
    let state = Rc::clone(&viewer);
    ui.on_settings_changed(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut viewer = state.borrow_mut();
        let old_theme = viewer.settings.theme;
        viewer.settings = Settings::from_ui(&ui);
        viewer.settings.save();
        if viewer.settings.theme != old_theme {
            viewer.update_blocks(&ui);
        }
    });

    let weak = ui.as_weak();
    let state = Rc::clone(&viewer);
    ui.on_reload_requested(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        state.borrow_mut().reload(&ui);
    });

    ui.on_open_link(|url| {
        let _ = std::process::Command::new("xdg-open")
            .arg(url.as_str())
            .spawn();
    });

    let state = Rc::clone(&viewer);
    ui.window().on_close_requested(move || {
        state.borrow().settings.save();
        slint::CloseRequestResponse::HideWindow
    });

    ui.run()
}

fn window_title(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!("{name} - lantai")
}

fn load(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
}

/// GTK's default interface font size in points, used when the desktop
/// settings cannot be read.
const DEFAULT_FONT_SIZE_PT: f32 = 11.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Theme {
    Dark,
    Light,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    theme: Theme,
    accent: [u8; 3],
    /// Interface font size in points.
    font_size: f32,
    /// Text color override; `None` uses the theme's default.
    text_color: Option<[u8; 3]>,
    /// Background color override; `None` uses pure black (dark theme) or
    /// pure white (light theme).
    background: Option<[u8; 3]>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            accent: [79, 123, 208],
            font_size: desktop_font_size_pt(),
            text_color: None,
            background: None,
        }
    }
}

impl Settings {
    fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        match rmsd_yaml::from_str(&text) {
            Ok(settings) => settings,
            Err(err) => {
                eprintln!("failed to parse {}: {err}", path.display());
                Self::default()
            }
        }
    }

    fn save(&self) {
        let Some(path) = config_path() else {
            return;
        };
        let Ok(text) = rmsd_yaml::to_string(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(err) = fs::write(&path, text) {
            eprintln!("failed to write {}: {err}", path.display());
        }
    }

    fn from_ui(ui: &AppWindow) -> Self {
        Self {
            theme: if ui.get_dark() {
                Theme::Dark
            } else {
                Theme::Light
            },
            accent: [
                channel(ui.get_accent_r()),
                channel(ui.get_accent_g()),
                channel(ui.get_accent_b()),
            ],
            font_size: ui.get_font_size() * 3.0 / 4.0,
            text_color: ui.get_custom_fg().then(|| {
                [
                    channel(ui.get_fg_r()),
                    channel(ui.get_fg_g()),
                    channel(ui.get_fg_b()),
                ]
            }),
            background: ui.get_custom_bg().then(|| {
                [
                    channel(ui.get_bg_r()),
                    channel(ui.get_bg_g()),
                    channel(ui.get_bg_b()),
                ]
            }),
        }
    }
}

fn channel(value: f32) -> u8 {
    value.clamp(0.0, 255.0).round() as u8
}

fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME").map(|home| Path::new(&home).join(".config"))
        })?;
    Some(base.join("lantai.yml"))
}

/// Wayland has no protocol for text scaling or font DPI: every toolkit reads
/// its desktop's settings itself. Match the configured interface font so text
/// comes out roughly the same size as in GTK/Qt apps.
fn desktop_font_size_pt() -> f32 {
    let size = gsettings_get("org.gnome.desktop.interface", "font-name")
        .and_then(|name| parse_font_size(&name))
        .unwrap_or(DEFAULT_FONT_SIZE_PT);
    let scaling =
        gsettings_get("org.gnome.desktop.interface", "text-scaling-factor")
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|scaling| *scaling > 0.0)
            .unwrap_or(1.0);
    size * scaling
}

/// Extracts the point size from a font description like `Noto Sans 11`.
fn parse_font_size(font_name: &str) -> Option<f32> {
    font_name.split_whitespace().last()?.parse().ok()
}

fn gsettings_get(schema: &str, key: &str) -> Option<String> {
    let output = std::process::Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    Some(value.trim().trim_matches('\'').to_owned())
}

struct Viewer {
    path: PathBuf,
    document: Result<String, String>,
    settings: Settings,
}

impl Viewer {
    fn new(path: PathBuf, settings: Settings) -> Self {
        let document = load(&path);
        Self {
            path,
            document,
            settings,
        }
    }

    fn title(&self) -> String {
        window_title(&self.path)
    }

    fn apply_to_ui(&self, ui: &AppWindow) {
        let settings = &self.settings;
        ui.set_font_size(settings.font_size * 4.0 / 3.0);
        ui.set_dark(settings.theme == Theme::Dark);

        let accent = settings.accent;
        ui.set_accent_r(f32::from(accent[0]));
        ui.set_accent_g(f32::from(accent[1]));
        ui.set_accent_b(f32::from(accent[2]));

        let [fg_r, fg_g, fg_b] = settings
            .text_color
            .unwrap_or_else(|| default_text_color(settings.theme));
        ui.set_custom_fg(settings.text_color.is_some());
        ui.set_fg_r(f32::from(fg_r));
        ui.set_fg_g(f32::from(fg_g));
        ui.set_fg_b(f32::from(fg_b));

        let [bg_r, bg_g, bg_b] = settings
            .background
            .unwrap_or_else(|| default_background(settings.theme));
        ui.set_custom_bg(settings.background.is_some());
        ui.set_bg_r(f32::from(bg_r));
        ui.set_bg_g(f32::from(bg_g));
        ui.set_bg_b(f32::from(bg_b));
    }

    fn update_blocks(&self, ui: &AppWindow) {
        let dark = self.settings.theme == Theme::Dark;
        let base_dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let blocks = match &self.document {
            Ok(text) => markdown::blocks_from_markdown(text, base_dir, dark),
            Err(err) => {
                vec![markdown::plain_block(&format!(
                    "failed to read file: {err}"
                ))]
            }
        };
        ui.set_blocks(ModelRc::new(VecModel::from(blocks)));
    }

    fn reload(&mut self, ui: &AppWindow) {
        self.document = load(&self.path);
        self.update_blocks(ui);
    }
}

fn default_text_color(theme: Theme) -> [u8; 3] {
    match theme {
        Theme::Dark => [212, 212, 212],
        Theme::Light => [36, 36, 36],
    }
}

fn default_background(theme: Theme) -> [u8; 3] {
    match theme {
        Theme::Dark => [0, 0, 0],
        Theme::Light => [255, 255, 255],
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, Theme, parse_font_size};

    #[test]
    fn settings_round_trip() {
        let settings = Settings {
            theme: Theme::Light,
            accent: [12, 34, 56],
            font_size: 13.5,
            text_color: Some([80, 220, 100]),
            background: Some([10, 20, 30]),
        };
        let text = rmsd_yaml::to_string(&settings).unwrap();
        let parsed: Settings = rmsd_yaml::from_str(&text).unwrap();
        assert_eq!(parsed.theme, Theme::Light);
        assert_eq!(parsed.accent, [12, 34, 56]);
        assert_eq!(parsed.font_size, 13.5);
        assert_eq!(parsed.text_color, Some([80, 220, 100]));
        assert_eq!(parsed.background, Some([10, 20, 30]));
    }

    #[test]
    fn partial_settings_use_defaults() {
        let parsed: Settings = rmsd_yaml::from_str("font_size: 20\n").unwrap();
        assert_eq!(parsed.theme, Theme::Dark);
        assert_eq!(parsed.font_size, 20.0);
        assert_eq!(parsed.text_color, None);
        assert_eq!(parsed.background, None);
    }

    #[test]
    fn parses_font_size_from_font_name() {
        assert_eq!(parse_font_size("Adwaita Sans 11"), Some(11.0));
        assert_eq!(parse_font_size("Noto Sans CJK SC 10.5"), Some(10.5));
        assert_eq!(parse_font_size("Sans"), None);
    }
}
