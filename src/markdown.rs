use std::{ops::Range, path::Path, sync::OnceLock};

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use slint::{Image, StyledText};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use unicode_width::UnicodeWidthStr;

use crate::Block;

pub const KIND_PARAGRAPH: i32 = 0;
pub const KIND_HEADING: i32 = 1;
pub const KIND_CODE: i32 = 2;
pub const KIND_LIST: i32 = 3;
pub const KIND_QUOTE: i32 = 4;
pub const KIND_RULE: i32 = 5;
pub const KIND_TABLE: i32 = 6;
pub const KIND_IMAGE: i32 = 7;

const PARSER_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

pub fn plain_block(text: &str) -> Block {
    Block {
        kind: KIND_PARAGRAPH,
        level: 0,
        styled: StyledText::from_plain_text(text),
        image: Image::default(),
        has_image: false,
    }
}

/// Converts markdown into renderable blocks, resolving relative image paths
/// against `base_dir` and highlighting code with a theme matching `dark`.
pub fn blocks_from_markdown(
    text: &str,
    base_dir: &Path,
    dark: bool,
) -> Vec<Block> {
    let syntaxes = syntaxes();
    let theme = theme_for(dark, themes());

    let events: Vec<(Event, Range<usize>)> =
        Parser::new_ext(text, PARSER_OPTIONS)
            .into_offset_iter()
            .collect();

    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut pending: Option<(Pending, usize)> = None;

    for (event, range) in &events {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    pending = Some((Pending::from_tag(tag), range.start));
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some((pending, start)) = pending.take()
                {
                    push_block(
                        &mut blocks,
                        pending,
                        &text[start..range.end],
                        base_dir,
                        syntaxes,
                        theme,
                    );
                }
            }
            Event::Rule if depth == 0 => {
                blocks.push(plain_kind(KIND_RULE, StyledText::default()));
            }
            _ => {}
        }
    }

    blocks
}

fn plain_kind(kind: i32, styled: StyledText) -> Block {
    Block {
        kind,
        level: 0,
        styled,
        image: Image::default(),
        has_image: false,
    }
}

enum Pending {
    Paragraph,
    Heading(u8),
    Code(String),
    List,
    Quote,
    Table,
    Other,
}

impl Pending {
    fn from_tag(tag: &Tag) -> Self {
        match tag {
            Tag::Paragraph => Self::Paragraph,
            Tag::Heading { level, .. } => Self::Heading(*level as u8),
            Tag::CodeBlock(kind) => Self::Code(match kind {
                CodeBlockKind::Fenced(info) => info.to_string(),
                CodeBlockKind::Indented => String::new(),
            }),
            Tag::List(_) => Self::List,
            Tag::BlockQuote(_) => Self::Quote,
            Tag::Table(_) => Self::Table,
            _ => Self::Other,
        }
    }
}

fn push_block(
    blocks: &mut Vec<Block>,
    pending: Pending,
    src: &str,
    base_dir: &Path,
    syntaxes: &SyntaxSet,
    theme: &Theme,
) {
    match pending {
        Pending::Paragraph => {
            if let Some((alt, dest)) = image_of(src) {
                if let Some(image) = load_image(&dest, base_dir) {
                    blocks.push(Block {
                        kind: KIND_IMAGE,
                        level: 0,
                        styled: StyledText::default(),
                        image,
                        has_image: true,
                    });
                    return;
                }
                blocks.push(plain_block(&alt));
                return;
            }
            blocks.push(styled_block(KIND_PARAGRAPH, 0, src));
        }
        Pending::Heading(level) => {
            blocks.push(styled_block(
                KIND_HEADING,
                level as i32,
                &heading_text(src),
            ));
        }
        Pending::Code(info) => {
            let lang = info.split_whitespace().next().unwrap_or("");
            let styled =
                highlight_code(&code_content(src), lang, syntaxes, theme);
            blocks.push(plain_kind(KIND_CODE, styled));
        }
        Pending::List => blocks.push(styled_block(KIND_LIST, 0, src)),
        Pending::Quote => {
            blocks.push(styled_block(KIND_QUOTE, 0, &unquote(src)));
        }
        Pending::Table => {
            let (header, rows) = parse_table(src);
            let table = render_table(&header, &rows);
            blocks.push(styled_block(KIND_TABLE, 0, &table));
        }
        Pending::Other => blocks.push(plain_block(src)),
    }
}

fn styled_block(kind: i32, level: i32, src: &str) -> Block {
    Block {
        kind,
        level,
        styled: styled_markdown(src),
        image: Image::default(),
        has_image: false,
    }
}

fn styled_markdown(src: &str) -> StyledText {
    StyledText::from_markdown(src)
        .unwrap_or_else(|_| StyledText::from_plain_text(src))
}

fn heading_text(src: &str) -> String {
    src.trim()
        .trim_start_matches('#')
        .trim()
        .trim_end_matches('#')
        .trim_end()
        .to_owned()
}

fn unquote(src: &str) -> String {
    src.lines()
        .map(|line| {
            let line = line.trim_start();
            match line.strip_prefix('>') {
                Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts the code inside a fenced or indented code block.
fn code_content(src: &str) -> String {
    let mut code = String::new();
    for event in Parser::new_ext(src, PARSER_OPTIONS) {
        if let Event::Text(text) = event {
            code.push_str(&text);
        }
    }
    code
}

fn image_of(src: &str) -> Option<(String, String)> {
    let mut alt = String::new();
    let mut dest = None;
    let mut in_image = false;
    let mut other_text = false;

    for event in Parser::new_ext(src, PARSER_OPTIONS) {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                in_image = true;
                dest = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Image) => in_image = false,
            Event::Text(text) => {
                if in_image {
                    alt.push_str(&text);
                } else if !text.trim().is_empty() {
                    other_text = true;
                }
            }
            _ => {}
        }
    }

    if other_text {
        return None;
    }
    dest.map(|dest| (alt, dest))
}

fn load_image(dest: &str, base_dir: &Path) -> Option<Image> {
    if dest.contains("://") || dest.starts_with("data:") {
        return None;
    }
    Image::load_from_path(&base_dir.join(dest)).ok()
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn themes() -> &'static ThemeSet {
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    THEMES.get_or_init(ThemeSet::load_defaults)
}

fn theme_for(dark: bool, themes: &ThemeSet) -> &Theme {
    let name = if dark {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    };
    themes.themes.get(name).unwrap_or_else(|| {
        themes
            .themes
            .values()
            .next()
            .expect("syntect ships with default themes")
    })
}

fn highlight_code(
    code: &str,
    lang: &str,
    syntaxes: &SyntaxSet,
    theme: &Theme,
) -> StyledText {
    let syntax = syntaxes
        .find_syntax_by_token(lang)
        .or_else(|| syntaxes.find_syntax_by_extension(lang))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());

    let mut highlighted = String::new();
    let mut highlighter = HighlightLines::new(syntax, theme);

    for line in LinesWithEndings::from(code) {
        let Ok(ranges) = highlighter.highlight_line(line, syntaxes) else {
            return StyledText::from_plain_text(code);
        };
        for (style, piece) in ranges {
            let piece = piece
                .strip_suffix("\r\n")
                .or_else(|| piece.strip_suffix('\n'))
                .unwrap_or(piece);
            if piece.is_empty() {
                continue;
            }
            let color = style.foreground;
            highlighted.push_str(&format!(
                "<font color=\"#{:02x}{:02x}{:02x}\">{}</font>",
                color.r,
                color.g,
                color.b,
                escape_markdown(piece),
            ));
        }
        highlighted.push('\n');
    }

    StyledText::from_markdown(&highlighted)
        .unwrap_or_else(|_| StyledText::from_plain_text(code))
}

/// Escapes text so it renders literally inside a generated markdown string.
fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '#' | '!' | '~' | '|'
            | '-' | '+' | '(' | ')' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

fn parse_table(src: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();

    for event in Parser::new_ext(src, PARSER_OPTIONS) {
        match event {
            Event::Start(Tag::TableHead) => row.clear(),
            Event::End(TagEnd::TableHead) => {
                header = std::mem::take(&mut row);
            }
            Event::Start(Tag::TableRow) => row.clear(),
            Event::End(TagEnd::TableRow) => {
                rows.push(std::mem::take(&mut row));
            }
            Event::Start(Tag::TableCell) => cell.clear(),
            Event::End(TagEnd::TableCell) => {
                row.push(std::mem::take(&mut cell));
            }
            Event::Text(text) | Event::Code(text) => cell.push_str(&text),
            Event::SoftBreak | Event::HardBreak => cell.push(' '),
            _ => {}
        }
    }

    (header, rows)
}

/// Renders a table as monospace text with aligned columns.
fn render_table(header: &[String], rows: &[Vec<String>]) -> String {
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(0);

    let mut widths = vec![0usize; columns];
    for (column, cell) in header.iter().enumerate() {
        widths[column] =
            widths[column].max(UnicodeWidthStr::width(cell.as_str()));
    }
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] =
                widths[column].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }

    let mut table = String::new();
    push_table_row(&mut table, header, &widths);
    let separator = widths
        .iter()
        .map(|w| "-".repeat(w + 2))
        .collect::<Vec<_>>()
        .join("+");
    table.push_str(&separator);
    table.push('\n');
    for row in rows {
        push_table_row(&mut table, row, &widths);
    }
    table
}

fn push_table_row(table: &mut String, cells: &[String], widths: &[usize]) {
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            table.push_str(" | ");
        }
        let cell = cells.get(column).map(String::as_str).unwrap_or("");
        table.push_str(cell);
        let padding = width.saturating_sub(UnicodeWidthStr::width(cell));
        for _ in 0..padding {
            table.push(' ');
        }
    }
    table.push('\n');
}
