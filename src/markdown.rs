use std::{borrow::Cow, ops::Range, path::Path, sync::OnceLock};

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
        styled: StyledText::default(),
        text: text.into(),
        image: Image::default(),
        has_image: false,
    }
}

/// Converts markdown into renderable blocks, resolving relative image paths
/// against `base_dir` and highlighting code with a theme matching `dark`.
///
/// Slint renders every newline in a markdown string as a paragraph break, so
/// soft line breaks are joined with spaces here, the way CommonMark renders
/// them. Blank lines still separate paragraphs and hard breaks still break
/// the line.
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
    let joins = soft_break_joins(&events);

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
                    let src = block_source(text, &(start..range.end), &joins);
                    push_block(
                        &mut blocks,
                        pending,
                        &src,
                        base_dir,
                        syntaxes,
                        theme,
                    );
                }
            }
            Event::Rule if depth == 0 => {
                blocks.push(text_block(KIND_RULE, 0, String::new()));
            }
            _ => {}
        }
    }

    blocks
}

/// Source ranges that a soft line break covers: the newline itself plus any
/// indentation or block markers (such as `> `) in front of the next piece of
/// content. CommonMark renders these ranges as a single space.
fn soft_break_joins(events: &[(Event, Range<usize>)]) -> Vec<Range<usize>> {
    let mut joins = Vec::new();
    for (index, (event, break_range)) in events.iter().enumerate() {
        if !matches!(event, Event::SoftBreak) {
            continue;
        }
        let end = events[index + 1..]
            .iter()
            .map(|(_, range)| range.start)
            .find(|start| *start >= break_range.end)
            .unwrap_or(break_range.end);
        joins.push(break_range.start..end);
    }
    joins
}

/// Builds the markdown source of a single block, replacing soft line breaks
/// with spaces as CommonMark does. Hard breaks and blank lines are kept, so
/// they still start a new line when rendered.
fn block_source<'a>(
    text: &'a str,
    block: &Range<usize>,
    joins: &[Range<usize>],
) -> Cow<'a, str> {
    if !joins
        .iter()
        .any(|join| block.start <= join.start && join.end <= block.end)
    {
        return Cow::Borrowed(&text[block.clone()]);
    }

    let mut source = String::with_capacity(block.len());
    let mut position = block.start;
    for join in joins {
        if join.start < block.start || join.end > block.end {
            continue;
        }
        source.push_str(&text[position..join.start]);
        // The parser drops the whitespace in front of a soft break from the
        // text, so it must not be kept next to the replacement space.
        while source.ends_with([' ', '\t']) {
            source.pop();
        }
        source.push(' ');
        position = join.end;
    }
    source.push_str(&text[position..block.end]);
    Cow::Owned(source)
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
                        text: alt.into(),
                        image,
                        has_image: true,
                    });
                    return;
                }
                blocks.push(plain_block(&alt));
                return;
            }
            blocks.push(text_block(KIND_PARAGRAPH, 0, plain_text(src)));
        }
        Pending::Heading(level) => {
            blocks.push(text_block(
                KIND_HEADING,
                level as i32,
                plain_text(&heading_text(src)),
            ));
        }
        Pending::Code(info) => {
            let lang = info.split_whitespace().next().unwrap_or("");
            let code = code_content(src);
            let styled = highlight_code(&code, lang, syntaxes, theme);
            blocks.push(Block {
                kind: KIND_CODE,
                level: 0,
                styled,
                text: code.into(),
                image: Image::default(),
                has_image: false,
            });
        }
        Pending::List => blocks.push(text_block(KIND_LIST, 0, plain_text(src))),
        Pending::Quote => {
            blocks.push(text_block(KIND_QUOTE, 0, plain_text(&unquote(src))))
        }
        Pending::Table => {
            let (header, rows) = parse_table(src);
            let table = render_table(&header, &rows);
            blocks.push(text_block(KIND_TABLE, 0, table));
        }
        Pending::Other => blocks.push(plain_block(src)),
    }
}

fn text_block(kind: i32, level: i32, text: String) -> Block {
    Block {
        kind,
        level,
        styled: StyledText::default(),
        text: text.into(),
        image: Image::default(),
        has_image: false,
    }
}

/// Renders the markdown of a block as plain text: emphasis and code markers
/// are dropped and lists become bulleted or numbered lines. Soft line breaks
/// were already joined with spaces by [`block_source`], hard breaks are kept.
fn plain_text(src: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut lists: Vec<Option<u64>> = Vec::new();

    for event in Parser::new_ext(src, PARSER_OPTIONS) {
        match event {
            Event::Start(Tag::Item) => {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                let depth = lists.len().saturating_sub(1);
                line.push_str(&"    ".repeat(depth));
                match lists.last_mut() {
                    Some(Some(number)) => {
                        line.push_str(&format!("{number}. "));
                        *number += 1;
                    }
                    Some(None) => line.push_str(match depth % 3 {
                        0 => "• ",
                        1 => "◦ ",
                        _ => "▪ ",
                    }),
                    None => {}
                }
            }
            Event::End(TagEnd::Item | TagEnd::Paragraph) => {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
            }
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Text(text) => line.push_str(&text),
            Event::Code(code) => line.push_str(&code),
            Event::SoftBreak => line.push(' '),
            Event::HardBreak => {
                lines.push(std::mem::take(&mut line));
                line.push_str(&"    ".repeat(lists.len().saturating_sub(1)));
            }
            Event::TaskListMarker(checked) => {
                line.push_str(if checked { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }

    if !line.is_empty() {
        lines.push(line);
    }
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Normalizes a whole markdown snippet as if it were a single block, the
    /// same way `blocks_from_markdown` does for every top-level block.
    fn normalized(src: &str) -> String {
        let events: Vec<(Event, Range<usize>)> =
            Parser::new_ext(src, PARSER_OPTIONS)
                .into_offset_iter()
                .collect();
        let joins = soft_break_joins(&events);
        block_source(src, &(0..src.len()), &joins).into_owned()
    }

    #[test]
    fn soft_breaks_join_with_spaces() {
        assert_eq!(normalized("hello\nworld"), "hello world");
        assert_eq!(normalized("hello\r\nworld"), "hello world");
        assert_eq!(normalized("hello \nworld"), "hello world");
        assert_eq!(normalized("hello\n  world"), "hello world");
        assert_eq!(normalized("*em\nph*"), "*em ph*");
        assert_eq!(normalized("para \\* text\nmore"), "para \\* text more");
    }

    #[test]
    fn blank_lines_and_hard_breaks_are_kept() {
        assert_eq!(normalized("hello\n\nworld"), "hello\n\nworld");
        assert_eq!(normalized("hello  \nworld"), "hello  \nworld");
        assert_eq!(normalized("hello\\\nworld"), "hello\\\nworld");
        assert_eq!(
            normalized("```\nlet a = 1;\nlet b = 2;\n```"),
            "```\nlet a = 1;\nlet b = 2;\n```"
        );
    }

    #[test]
    fn list_items_and_nested_markers_join_correctly() {
        assert_eq!(
            normalized("- one\n  continued\n- two"),
            "- one continued\n- two"
        );
        assert_eq!(
            normalized("- one\n  > quote\n  > cont"),
            "- one\n  > quote cont"
        );
        assert_eq!(
            normalized("1. one\n   continued\n2. two"),
            "1. one continued\n2. two"
        );
    }

    #[test]
    fn blockquotes_join_after_unquoting() {
        assert_eq!(
            unquote(&normalized("> quote one\n> quote cont\n>\n> second para")),
            "quote one quote cont\n\nsecond para"
        );
    }

    #[test]
    fn paragraphs_render_soft_breaks_as_spaces() {
        let blocks = blocks_from_markdown("hello\nworld", Path::new("."), true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, KIND_PARAGRAPH);
        assert_eq!(blocks[0].text, "hello world");
    }

    #[test]
    fn blank_lines_still_separate_blocks() {
        let blocks = blocks_from_markdown("one\n\ntwo", Path::new("."), true);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "one");
        assert_eq!(blocks[1].text, "two");
    }

    #[test]
    fn inline_markers_are_dropped_from_plain_text() {
        assert_eq!(
            plain_text("a **bold** and *italic* and `code` word"),
            "a bold and italic and code word"
        );
        assert_eq!(plain_text("[label](https://example.com)"), "label");
    }

    #[test]
    fn lists_become_bulleted_and_numbered_lines() {
        assert_eq!(plain_text("- one\n- two"), "• one\n• two");
        assert_eq!(plain_text("1. one\n2. two"), "1. one\n2. two");
        assert_eq!(
            plain_text("- one\n  - nested\n- two"),
            "• one\n    ◦ nested\n• two"
        );
    }

    #[test]
    fn hard_breaks_are_kept_in_plain_text() {
        assert_eq!(plain_text("one  \ntwo"), "one\ntwo");
        assert_eq!(plain_text("one\\\ntwo"), "one\ntwo");
    }

    #[test]
    fn code_blocks_keep_their_source_for_copying() {
        let blocks = blocks_from_markdown(
            "```rust\nlet a = 1;\n```",
            Path::new("."),
            true,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, KIND_CODE);
        assert_eq!(blocks[0].text, "let a = 1;\n");
        assert_ne!(blocks[0].styled, StyledText::default());
    }
}
