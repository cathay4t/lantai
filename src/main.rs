mod markdown;

use std::{
    cell::RefCell,
    env, fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, VecModel};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let Some(path) = env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: lantai <file.md>");
        std::process::exit(2);
    };
    let path = fs::canonicalize(&path).unwrap_or(path);

    let ui = AppWindow::new()?;
    ui.set_mono_family(monospace_family().into());
    let settings = Settings::load();
    let viewer = Rc::new(RefCell::new(Viewer::new(path, settings)));
    let block_positions = Rc::new(RefCell::new(Vec::<f32>::new()));

    {
        let mut viewer = viewer.borrow_mut();
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

    let state = Rc::clone(&block_positions);
    ui.on_block_positioned(move |index, y| {
        let index = index.max(0) as usize;
        let mut positions = state.borrow_mut();
        if positions.len() <= index {
            positions.resize(index + 1, 0.0);
        }
        positions[index] = y;
    });

    let weak = ui.as_weak();
    let state = Rc::clone(&viewer);
    let positions = Rc::clone(&block_positions);
    ui.on_search_requested(move |query, forward| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let (result, matches, current) = {
            let mut viewer = state.borrow_mut();
            let result = viewer.search(query.as_str(), forward);
            (result, viewer.search_matches.clone(), viewer.search_from)
        };
        update_block_highlights(
            &ui,
            &matches,
            if result.is_some() { current } else { None },
        );
        match result {
            Some(hit) => {
                ui.set_search_status(
                    format!("{}/{}", hit.ordinal, hit.total).into(),
                );
                ui.set_search_match(hit.block as i32);
                ui.set_search_highlight_range(ModelRc::new(VecModel::from(
                    vec![hit.start as i32, hit.end as i32],
                )));
                if let Some(&y) = positions.borrow().get(hit.block) {
                    ui.invoke_scroll_to_position(y);
                }
            }
            None => {
                ui.set_search_status("no matches".into());
                ui.set_search_match(-1);
                ui.set_search_highlight_range(ModelRc::new(VecModel::from(
                    Vec::<i32>::new(),
                )));
            }
        }
    });

    let state = Rc::clone(&viewer);
    ui.on_quit_requested(move || {
        state.borrow().settings.save();
        let _ = slint::quit_event_loop();
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

/// Resolves the platform's preferred monospace family.
///
/// Slint looks up `default-font-family` as a family *name*, so the CSS generic
/// keyword `"monospace"` falls back to the proportional default font. Asking
/// the font collection for the generic family yields a concrete name that
/// Slint can match.
fn monospace_family() -> String {
    let mut collection = slint::fontique_011::shared_collection();
    let families: Vec<_> = collection
        .generic_families(
            slint::fontique_011::fontique::GenericFamily::Monospace,
        )
        .collect();
    families
        .first()
        .and_then(|family| collection.family_name(*family))
        .map(str::to_owned)
        .unwrap_or_else(|| "monospace".to_owned())
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
    block_texts: Vec<String>,
    search_query: String,
    search_matches: Vec<SearchMatch>,
    search_from: Option<usize>,
}

#[derive(Clone, Copy)]
struct SearchMatch {
    block: usize,
    start: usize,
    end: usize,
}

impl Viewer {
    fn new(path: PathBuf, settings: Settings) -> Self {
        let document = load(&path);
        Self {
            path,
            document,
            settings,
            block_texts: Vec::new(),
            search_query: String::new(),
            search_matches: Vec::new(),
            search_from: None,
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

    fn update_blocks(&mut self, ui: &AppWindow) {
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
        self.block_texts =
            blocks.iter().map(|block| block.text.to_string()).collect();
        self.search_query.clear();
        self.search_matches.clear();
        self.search_from = None;
        ui.set_search_match(-1);
        ui.set_search_highlight_range(ModelRc::new(VecModel::from(
            Vec::<i32>::new(),
        )));
        ui.set_blocks(ModelRc::new(VecModel::from(blocks)));
    }

    fn reload(&mut self, ui: &AppWindow) {
        self.document = load(&self.path);
        self.update_blocks(ui);
    }

    /// Finds the next occurrence of `query` in the requested direction.
    fn search(&mut self, query: &str, forward: bool) -> Option<SearchHit> {
        if query != self.search_query {
            self.search_query = query.to_owned();
            self.search_matches =
                find_all_case_insensitive(&self.block_texts, query);
            self.search_from = None;
        }
        let total = self.search_matches.len();
        if total == 0 {
            return None;
        }

        let index = match (self.search_from, forward) {
            (None, true) => 0,
            (None, false) => total - 1,
            (Some(index), true) => (index + 1) % total,
            (Some(index), false) => (index + total - 1) % total,
        };
        self.search_from = Some(index);

        let hit = self.search_matches[index];
        Some(SearchHit {
            block: hit.block,
            ordinal: index + 1,
            total,
            start: hit.start,
            end: hit.end,
        })
    }
}

struct SearchHit {
    block: usize,
    ordinal: usize,
    total: usize,
    start: usize,
    end: usize,
}

/// Stores every non-focused match in its block so the UI can paint them all,
/// with the focused match handled separately by `search-match`.
fn update_block_highlights(
    ui: &AppWindow,
    matches: &[SearchMatch],
    current: Option<usize>,
) {
    let model = ui.get_blocks();
    let count = model.row_count();
    let mut per_block: Vec<Vec<SearchHighlight>> = vec![Vec::new(); count];
    for (index, hit) in matches.iter().enumerate() {
        if current == Some(index) || hit.block >= count {
            continue;
        }
        per_block[hit.block].push(SearchHighlight {
            start: hit.start as i32,
            end: hit.end as i32,
        });
    }

    for (index, ranges) in per_block.into_iter().enumerate() {
        let Some(mut block) = model.row_data(index) else {
            continue;
        };
        let unchanged = block.highlights.row_count() == ranges.len()
            && block.highlights.iter().zip(&ranges).all(|(have, want)| {
                have.start == want.start && have.end == want.end
            });
        if unchanged {
            continue;
        }
        block.highlights = ModelRc::new(VecModel::from(ranges));
        model.set_row_data(index, block);
    }
}

/// Finds every occurrence of `needle` across `blocks`, ignoring case, in
/// document order. Ranges are byte offsets into each block's text.
fn find_all_case_insensitive(
    blocks: &[String],
    needle: &str,
) -> Vec<SearchMatch> {
    let needle = needle.to_lowercase();
    let mut matches = Vec::new();
    if needle.is_empty() {
        return matches;
    }
    for (block, text) in blocks.iter().enumerate() {
        for (start, end) in find_ranges(text, &needle) {
            matches.push(SearchMatch { block, start, end });
        }
    }
    matches
}

/// Returns the byte ranges in `haystack` that match the already-lowercased
/// `needle`, mapping positions in the lowercased text back to the original.
fn find_ranges(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut lowered = String::with_capacity(haystack.len());
    // Maps each byte offset in `lowered` to the byte offset of the original
    // character that produced it, plus one entry for the end of the text.
    let mut boundaries = Vec::with_capacity(haystack.len() + 1);
    for (offset, ch) in haystack.char_indices() {
        boundaries.push((lowered.len(), offset));
        for lower in ch.to_lowercase() {
            lowered.push(lower);
        }
    }
    boundaries.push((lowered.len(), haystack.len()));

    let map_offset = |lower: usize| {
        boundaries
            .binary_search_by_key(&lower, |&(lo, _)| lo)
            .ok()
            .map(|index| boundaries[index].1)
    };

    let mut ranges = Vec::new();
    let mut search_from = 0;
    while let Some(position) = lowered[search_from..].find(needle) {
        let start = search_from + position;
        let end = start + needle.len();
        if let (Some(start), Some(end)) = (map_offset(start), map_offset(end)) {
            ranges.push((start, end));
        }
        search_from = end.max(start + 1);
    }
    ranges
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
    use super::{Settings, Theme, find_all_case_insensitive, parse_font_size};

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

    #[test]
    fn finds_case_insensitive_match_ranges() {
        let blocks = |texts: &[&str]| {
            texts
                .iter()
                .map(|text| text.to_string())
                .collect::<Vec<_>>()
        };
        let ranges = |texts: &[&str], needle: &str| {
            find_all_case_insensitive(&blocks(texts), needle)
                .into_iter()
                .map(|m| (m.block, m.start, m.end))
                .collect::<Vec<_>>()
        };

        assert_eq!(ranges(&["hello wait world"], "wait"), vec![(0, 6, 10)]);
        assert_eq!(ranges(&["Waiting here"], "wait"), vec![(0, 0, 4)]);
        assert_eq!(ranges(&["nothing here"], "wait"), Vec::new());
        assert_eq!(ranges(&["héllo wáit"], "wáit"), vec![(0, 7, 12)]);
        assert_eq!(
            ranges(&["wait and wait", "wait"], "wait"),
            vec![(0, 0, 4), (0, 9, 13), (1, 0, 4)]
        );
    }
}
