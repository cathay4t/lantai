use std::{borrow::Cow, ops::Range, path::Path, sync::OnceLock};

use pulldown_cmark::{
    BlockQuoteKind, CodeBlockKind, Event, Options, Parser, Tag, TagEnd,
};
use slint::{Image, ModelRc, StyledText, VecModel};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use unicode_width::UnicodeWidthStr;

use crate::{Block, TableCell, TableRow};

pub const KIND_PARAGRAPH: i32 = 0;
pub const KIND_HEADING: i32 = 1;
pub const KIND_CODE: i32 = 2;
pub const KIND_LIST: i32 = 3;
pub const KIND_QUOTE: i32 = 4;
pub const KIND_RULE: i32 = 5;
pub const KIND_TABLE: i32 = 6;
pub const KIND_IMAGE: i32 = 7;
pub const KIND_ALERT: i32 = 8;
pub const KIND_FOOTNOTE: i32 = 9;

const PARSER_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_GFM);

pub fn plain_block(text: &str) -> Block {
    Block {
        kind: KIND_PARAGRAPH,
        level: 0,
        styled: StyledText::from_plain_text(text),
        text: text.into(),
        image: Image::default(),
        has_image: false,
        table: Default::default(),
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

    // Footnotes are defined in a separate block, so re-parsing a block on its
    // own would not resolve its references. Replace them up front with their
    // Unicode marker, which is also what the styled renderer can display.
    let resolved = resolve_footnote_references(text);
    let text = resolved.as_str();

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

    // Footnote definitions render at the end of the document, regardless of
    // where they appear in the source.
    let mut footnotes = Vec::new();
    blocks.retain(|block| {
        if block.kind == KIND_FOOTNOTE {
            footnotes.push(block.clone());
            false
        } else {
            true
        }
    });
    blocks.extend(footnotes);

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
    Alert(i32),
    Table,
    Footnote(String),
    Html,
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
            Tag::BlockQuote(Some(kind)) => Self::Alert(alert_level(*kind)),
            Tag::BlockQuote(None) => Self::Quote,
            Tag::Table(_) => Self::Table,
            Tag::FootnoteDefinition(name) => Self::Footnote(name.to_string()),
            Tag::HtmlBlock => Self::Html,
            _ => Self::Other,
        }
    }
}

fn alert_level(kind: BlockQuoteKind) -> i32 {
    match kind {
        BlockQuoteKind::Note => 1,
        BlockQuoteKind::Tip => 2,
        BlockQuoteKind::Important => 3,
        BlockQuoteKind::Warning => 4,
        BlockQuoteKind::Caution => 5,
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
                        table: Default::default(),
                    });
                    return;
                }
                blocks.push(plain_block(&alt));
                return;
            }
            blocks.push(rich_block(KIND_PARAGRAPH, 0, src, false));
        }
        Pending::Heading(level) => {
            blocks.push(rich_block(KIND_HEADING, level as i32, src, true));
        }
        Pending::Code(info) => {
            let lang = info.split_whitespace().next().unwrap_or("");
            let code = code_content(src);
            blocks.push(code_block(&code, lang, syntaxes, theme));
        }
        Pending::List => push_list_blocks(blocks, src, syntaxes, theme),
        Pending::Quote => {
            let body = quote_body(src, false);
            blocks.push(rich_block(KIND_QUOTE, 0, &body, false));
        }
        Pending::Alert(level) => {
            let body = quote_body(src, true);
            blocks.push(rich_block(KIND_ALERT, level, &body, false));
        }
        Pending::Footnote(name) => {
            let body = plain_text(src);
            let styled_body = styled_markdown(src);
            let marker = superscript(&name);
            let plain = format!("{marker} {body}");
            let styled_source = format!("**{marker}** {styled_body}");
            blocks.push(Block {
                kind: KIND_FOOTNOTE,
                level: 0,
                styled: styled_or_plain(&styled_source, &plain),
                text: plain.into(),
                image: Image::default(),
                has_image: false,
                table: Default::default(),
            });
        }
        Pending::Table => {
            let (header, rows) = parse_table(src);
            let table = render_table(&header, &rows);
            let rows = std::iter::once((header, true))
                .chain(rows.into_iter().map(|cells| (cells, false)))
                .map(|(cells, header)| TableRow {
                    header,
                    cells: ModelRc::new(VecModel::from(
                        cells
                            .into_iter()
                            .map(|text| TableCell { text: text.into() })
                            .collect::<Vec<_>>(),
                    )),
                })
                .collect::<Vec<_>>();
            blocks.push(Block {
                kind: KIND_TABLE,
                level: 0,
                styled: StyledText::default(),
                text: table.into(),
                image: Image::default(),
                has_image: false,
                table: ModelRc::new(VecModel::from(rows)),
            });
        }
        Pending::Html => {
            let trimmed = src.trim();
            if !trimmed.starts_with("<!--") {
                blocks.push(plain_block(trimmed));
            }
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
        table: Default::default(),
    }
}

/// Builds a block that keeps its inline markdown formatting through Slint's
/// styled-text renderer, while `text` stays plain for selection and copying.
fn rich_block(kind: i32, level: i32, src: &str, bold: bool) -> Block {
    let plain = plain_text(src);
    let mut styled_source = styled_markdown(src);
    if bold && !styled_source.trim().is_empty() {
        styled_source = format!("**{}**", styled_source.trim());
    }
    Block {
        kind,
        level,
        styled: styled_or_plain(&styled_source, &plain),
        text: plain.into(),
        image: Image::default(),
        has_image: false,
        table: Default::default(),
    }
}

fn styled_or_plain(styled_source: &str, plain: &str) -> StyledText {
    StyledText::from_markdown(styled_source)
        .unwrap_or_else(|_| StyledText::from_plain_text(plain))
}

/// Renders the inline markdown of a block as Slint styled-text markdown,
/// preserving emphasis, strikethrough, inline code, links and underline.
fn styled_markdown(src: &str) -> String {
    let mut out = String::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut sub = 0usize;
    let mut sup = 0usize;

    for event in Parser::new_ext(src, PARSER_OPTIONS) {
        match event {
            Event::Start(Tag::Item) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                let depth = lists.len().saturating_sub(1);
                out.push_str(&"    ".repeat(depth));
                match lists.last_mut() {
                    Some(Some(number)) => {
                        out.push_str(&format!("{number}. "));
                        *number += 1;
                    }
                    Some(None) => out.push_str(match depth % 3 {
                        0 => "• ",
                        1 => "◦ ",
                        _ => "▪ ",
                    }),
                    None => {}
                }
            }
            Event::End(TagEnd::Item | TagEnd::Paragraph) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Emphasis) => out.push('*'),
            Event::End(TagEnd::Emphasis) => out.push('*'),
            Event::Start(Tag::Strong) => out.push_str("**"),
            Event::End(TagEnd::Strong) => out.push_str("**"),
            Event::Start(Tag::Strikethrough) => out.push_str("~~"),
            Event::End(TagEnd::Strikethrough) => out.push_str("~~"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                out.push('[');
                links.push(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                let destination = links.pop().unwrap_or_default();
                out.push_str(&link_close(&destination));
            }
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => {}
            Event::Text(text) => {
                push_text(&mut out, &text, sub > 0, sup > 0);
            }
            Event::Code(code) => out.push_str(&code_span(&code)),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push('\n'),
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "☑ " } else { "☐ " });
            }
            Event::FootnoteReference(name) => {
                out.push_str(&superscript(&name));
            }
            Event::InlineHtml(html) => {
                push_inline_html(&mut out, &html, &mut sub, &mut sup);
            }
            _ => {}
        }
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Emits markdown text, escaping characters that would otherwise be parsed as
/// formatting. Text inside `<sub>`/`<sup>` is converted to Unicode glyphs,
/// because Slint's styled-text renderer has no subscript or superscript style.
fn push_text(out: &mut String, text: &str, sub: bool, sup: bool) {
    for c in text.chars() {
        let c = if sub {
            subscript_char(c)
        } else if sup {
            superscript_char(c)
        } else {
            c
        };
        push_escaped(out, c);
    }
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '\\' | '`' | '*' | '_' | '[' | ']' | '#' | '!' | '~' | '|' | '-'
        | '+' | '(' | ')' => {
            out.push('\\');
            out.push(c);
        }
        _ => out.push(c),
    }
}

/// Handles the handful of inline HTML tags GitHub documents. Unknown tags are
/// dropped: Slint would otherwise reject the whole styled-text paragraph.
fn push_inline_html(
    out: &mut String,
    html: &str,
    sub: &mut usize,
    sup: &mut usize,
) {
    let tag = html.trim().to_ascii_lowercase();
    match tag.as_str() {
        "<sub>" => *sub += 1,
        "</sub>" => *sub = sub.saturating_sub(1),
        "<sup>" => *sup += 1,
        "</sup>" => *sup = sup.saturating_sub(1),
        "<ins>" | "<u>" => out.push_str("<u>"),
        "</ins>" | "</u>" => out.push_str("</u>"),
        "<b>" | "<strong>" => out.push_str("**"),
        "</b>" | "</strong>" => out.push_str("**"),
        "<i>" | "<em>" => out.push('*'),
        "</i>" | "</em>" => out.push('*'),
        "<del>" | "<s>" | "<strike>" => out.push_str("~~"),
        "</del>" | "</s>" | "</strike>" => out.push_str("~~"),
        "<br>" | "<br/>" | "<br />" => out.push('\n'),
        _ => {}
    }
}

/// Builds the `](destination)` closing of a link.
fn link_close(dest: &str) -> String {
    if dest.is_empty() {
        return "](".to_owned();
    }
    let needs_angle = dest.contains([' ', '\t', '(', ')', '<', '>']);
    if needs_angle {
        let mut destination = String::from("](<");
        for c in dest.chars() {
            match c {
                '<' => destination.push_str("%3C"),
                '>' => destination.push_str("%3E"),
                ' ' => destination.push_str("%20"),
                '\t' => destination.push_str("%09"),
                _ => destination.push(c),
            }
        }
        destination.push_str(">)");
        destination
    } else {
        format!("]({dest})")
    }
}

/// Renders the markdown of a block as plain text: emphasis and code markers
/// are dropped and lists become bulleted or numbered lines. Soft line breaks
/// were already joined with spaces by [`block_source`], hard breaks are kept.
fn plain_text(src: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut sub = 0usize;
    let mut sup = 0usize;

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
            Event::Text(text) => {
                for c in text.chars() {
                    line.push(if sub > 0 {
                        subscript_char(c)
                    } else if sup > 0 {
                        superscript_char(c)
                    } else {
                        c
                    });
                }
            }
            Event::Code(code) => line.push_str(&code),
            Event::SoftBreak => line.push(' '),
            Event::HardBreak => {
                lines.push(std::mem::take(&mut line));
                line.push_str(&"    ".repeat(lists.len().saturating_sub(1)));
            }
            Event::TaskListMarker(checked) => {
                line.push_str(if checked { "☑ " } else { "☐ " });
            }
            Event::FootnoteReference(name) => {
                line.push_str(&superscript(&name))
            }
            Event::InlineHtml(html) => {
                push_plain_inline_html(&html, &mut sub, &mut sup, &mut line);
            }
            _ => {}
        }
    }

    if !line.is_empty() {
        lines.push(line);
    }
    lines.join("\n")
}

fn push_plain_inline_html(
    html: &str,
    sub: &mut usize,
    sup: &mut usize,
    line: &mut String,
) {
    let tag = html.trim().to_ascii_lowercase();
    match tag.as_str() {
        "<sub>" => *sub += 1,
        "</sub>" => *sub = sub.saturating_sub(1),
        "<sup>" => *sup += 1,
        "</sup>" => *sup = sup.saturating_sub(1),
        "<br>" | "<br/>" | "<br />" => line.push('\n'),
        _ => {}
    }
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

/// Removes the blockquote markers, and for alerts the `[!NOTE]` style first
/// line, leaving just the alert body.
fn quote_body(src: &str, alert: bool) -> String {
    let body = unquote(src);
    if !alert {
        return body;
    }
    let mut lines = body.lines();
    match lines.next() {
        Some(first) if is_alert_marker(first.trim()) => {
            lines.collect::<Vec<_>>().join("\n")
        }
        _ => body,
    }
}

fn is_alert_marker(line: &str) -> bool {
    matches!(
        line.to_ascii_uppercase().as_str(),
        "[!NOTE]" | "[!TIP]" | "[!IMPORTANT]" | "[!WARNING]" | "[!CAUTION]"
    )
}

/// Wraps inline code in a backtick fence long enough for its contents.
fn code_span(code: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in code.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    format!("{fence} {code} {fence}")
}

fn superscript(text: &str) -> String {
    text.chars().map(superscript_char).collect()
}

/// Replaces `[^name]` references with Unicode superscript markers. The
/// replacement is driven by parser offsets, so references inside code spans or
/// code blocks are left untouched.
fn resolve_footnote_references(text: &str) -> String {
    let references: Vec<(Range<usize>, String)> =
        Parser::new_ext(text, PARSER_OPTIONS)
            .into_offset_iter()
            .filter_map(|(event, range)| match event {
                Event::FootnoteReference(name) => {
                    Some((range, superscript(&name)))
                }
                _ => None,
            })
            .collect();
    if references.is_empty() {
        return text.to_owned();
    }

    let mut resolved = String::with_capacity(text.len());
    let mut position = 0;
    for (range, marker) in references {
        resolved.push_str(&text[position..range.start]);
        resolved.push_str(&marker);
        position = range.end;
    }
    resolved.push_str(&text[position..]);
    resolved
}

fn superscript_char(c: char) -> char {
    match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        _ => c,
    }
}

fn subscript_char(c: char) -> char {
    match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => c,
    }
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

/// Finds code blocks nested inside a container such as a list item, returning
/// the byte range within `src`, the fence's info string, and the code itself.
fn nested_code_blocks(src: &str) -> Vec<(Range<usize>, String, String)> {
    let mut found = Vec::new();
    let mut open: Option<(usize, String, String)> = None;
    for (event, range) in
        Parser::new_ext(src, PARSER_OPTIONS).into_offset_iter()
    {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                open = Some((range.start, info, String::new()));
            }
            Event::Text(text) => {
                if let Some((_, _, code)) = open.as_mut() {
                    code.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start, info, code)) = open.take() {
                    found.push((start..range.end, info, code));
                }
            }
            _ => {}
        }
    }
    found
}

fn code_block(
    code: &str,
    lang: &str,
    syntaxes: &SyntaxSet,
    theme: &Theme,
) -> Block {
    Block {
        kind: KIND_CODE,
        level: 0,
        styled: highlight_code(code, lang, syntaxes, theme),
        text: code.into(),
        image: Image::default(),
        has_image: false,
        table: Default::default(),
    }
}

/// Splits a list into blocks, lifting any code blocks nested in its items out
/// into their own code blocks. Code inside a list item cannot survive being
/// flattened into the list's styled text.
fn push_list_blocks(
    blocks: &mut Vec<Block>,
    src: &str,
    syntaxes: &SyntaxSet,
    theme: &Theme,
) {
    let code_blocks = nested_code_blocks(src);
    if code_blocks.is_empty() {
        blocks.push(rich_block(KIND_LIST, 0, src, false));
        return;
    }

    let items = top_level_items(src);
    let Some(first) = items.first() else {
        blocks.push(rich_block(KIND_LIST, 0, src, false));
        return;
    };

    let mut group_start = first.start;
    for item in &items {
        let nested: Vec<_> = code_blocks
            .iter()
            .filter(|(range, _, _)| {
                item.start <= range.start && range.end <= item.end
            })
            .collect();
        if nested.is_empty() {
            continue;
        }

        // Keep the complete items before this one together so their list
        // spacing is unchanged.
        let before = src[group_start..item.start].trim_end();
        if !before.is_empty() {
            blocks.push(rich_block(KIND_LIST, 0, before, false));
        }

        let mut cursor = item.start;
        for (range, info, code) in nested {
            let before = src[cursor..range.start].trim_end();
            if !before.trim().is_empty() {
                blocks.push(rich_block(KIND_LIST, 0, before, false));
            }
            let lang = info.split_whitespace().next().unwrap_or("");
            blocks.push(code_block(code, lang, syntaxes, theme));
            cursor = range.end;
        }
        let after = src[cursor..item.end].trim_start_matches('\n');
        if !after.trim().is_empty() {
            blocks.push(rich_block(KIND_LIST, 0, after, false));
        }
        group_start = item.end;
    }

    let tail = src[group_start..].trim_end_matches('\n');
    if !tail.trim().is_empty() {
        blocks.push(rich_block(KIND_LIST, 0, tail, false));
    }
}

/// Byte ranges of the list's own items, ignoring items of nested lists.
fn top_level_items(src: &str) -> Vec<Range<usize>> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (event, range) in
        Parser::new_ext(src, PARSER_OPTIONS).into_offset_iter()
    {
        match event {
            Event::Start(Tag::List(_)) => depth += 1,
            Event::End(TagEnd::List(_)) => depth = depth.saturating_sub(1),
            Event::Start(Tag::Item) if depth == 1 => start = Some(range.start),
            Event::End(TagEnd::Item) if depth == 1 => {
                if let Some(start) = start.take() {
                    items.push(start..range.end);
                }
            }
            _ => {}
        }
    }
    items
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

    #[test]
    fn inline_styles_are_preserved_in_styled_markdown() {
        assert_eq!(
            styled_markdown("a **bold** and *italic* and ~~gone~~ and `code`"),
            "a **bold** and *italic* and ~~gone~~ and ` code `"
        );
        assert_eq!(
            styled_markdown("***all this text is important***"),
            "***all this text is important***"
        );
        assert_eq!(styled_markdown("<ins>under</ins>"), "<u>under</u>");
        assert_eq!(styled_markdown("a <b>bold</b> word"), "a **bold** word");
        assert!(
            StyledText::from_markdown(&styled_markdown(
                "a **bold** and *italic* and ~~gone~~ and `code`"
            ))
            .is_ok()
        );
    }

    #[test]
    fn links_are_preserved_in_styled_markdown() {
        assert_eq!(
            styled_markdown("[docs](https://example.com/a)"),
            "[docs](https://example.com/a)"
        );
        assert_eq!(
            styled_markdown("[a b](<https://example.com/a b>)"),
            "[a b](<https://example.com/a%20b>)"
        );
        assert!(
            StyledText::from_markdown(&styled_markdown(
                "[docs](https://example.com/a)"
            ))
            .is_ok()
        );
    }

    #[test]
    fn subscript_superscript_and_task_lists_render() {
        assert_eq!(plain_text("H<sub>2</sub>O"), "H₂O");
        assert_eq!(plain_text("E = mc<sup>2</sup>"), "E = mc²");
        assert_eq!(plain_text("- [x] done\n- [ ] todo"), "• ☑ done\n• ☐ todo");
        assert!(
            StyledText::from_markdown(&styled_markdown(
                "H<sub>2</sub>O and x<sup>2</sup>"
            ))
            .is_ok()
        );
    }

    #[test]
    fn headings_keep_inline_styles_and_drop_markers() {
        let blocks =
            blocks_from_markdown("# A *Helpful* Heading", Path::new("."), true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, KIND_HEADING);
        assert_eq!(blocks[0].level, 1);
        assert_eq!(blocks[0].text, "A Helpful Heading");
        assert_ne!(blocks[0].styled, StyledText::default());
    }

    #[test]
    fn generated_styled_markdown_parses_for_common_syntax() {
        let samples = [
            "a **bold** _italic_ ~~strike~~ `code` <ins>in</ins> <sub>s</sub> \
             <sup>p</sup>",
            "escaped \\*star\\* and \\[brackets\\]",
            "[link](https://example.com/a_b?q=1&x=2) and ![alt](img.png)",
            "***both*** and **bold _nested_ bold**",
            "a <kbd>Ctrl</kbd>+<kbd>B</kbd> combo",
            "line one  \nline two",
            "<!-- hidden --> visible",
            "H<sub>2</sub>O x<sup>2</sup>",
        ];
        for sample in samples {
            let generated = styled_markdown(sample);
            assert!(
                StyledText::from_markdown(&generated).is_ok(),
                "{sample:?} -> {generated:?}"
            );
        }
    }

    #[test]
    fn gfm_alerts_become_alert_blocks() {
        let blocks = blocks_from_markdown(
            "> [!WARNING]\n> This is *urgent*.",
            Path::new("."),
            true,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, KIND_ALERT);
        assert_eq!(blocks[0].level, 4);
        assert_eq!(blocks[0].text, "This is urgent.");
        assert_ne!(blocks[0].styled, StyledText::default());
    }

    #[test]
    fn alert_bodies_keep_their_block_content() {
        let blocks = blocks_from_markdown(
            "> [!TIP]\n> - one\n> - two",
            Path::new("."),
            true,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, KIND_ALERT);
        assert_eq!(blocks[0].level, 2);
        assert_eq!(blocks[0].text, "• one\n• two");
        assert_ne!(blocks[0].styled, StyledText::default());
    }

    #[test]
    fn footnotes_render_references_and_definitions() {
        let blocks = blocks_from_markdown(
            "Here is a footnote[^1].\n\n[^1]: My reference.",
            Path::new("."),
            true,
        );
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Here is a footnote¹.");
        assert_eq!(blocks[0].kind, KIND_PARAGRAPH);
        assert_eq!(blocks[1].text, "¹ My reference.");
        assert_eq!(blocks[1].kind, KIND_FOOTNOTE);
    }

    #[test]
    fn footnote_definitions_render_at_the_end() {
        let blocks = blocks_from_markdown(
            "[^1]: Defined first.\n\nBody[^1].\n\nLast paragraph.",
            Path::new("."),
            true,
        );
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2].kind, KIND_FOOTNOTE);
        assert_eq!(blocks[2].text, "¹ Defined first.");
    }

    #[test]
    fn html_comments_are_hidden() {
        let blocks = blocks_from_markdown(
            "before\n\n<!-- hidden -->\n\nafter",
            Path::new("."),
            true,
        );
        assert_eq!(
            blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(),
            ["before", "after"]
        );
    }

    #[test]
    fn plain_blocks_render_their_text() {
        let block = plain_block("failed to read file");
        assert_eq!(
            block.styled,
            StyledText::from_plain_text("failed to read file")
        );
    }
}

#[cfg(test)]
mod debug_tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn debug_file_table() {
        let home = std::env::var("HOME").unwrap();
        let md = std::fs::read_to_string(format!("{home}/linqu_backport.md"))
            .unwrap();
        let blocks = blocks_from_markdown(&md, Path::new("."), true);
        let mut tables = 0;
        for b in &blocks {
            if b.kind == 6 {
                tables += 1;
                println!(
                    "TABLE: {:?}",
                    &b.text.chars().take(80).collect::<String>()
                );
            }
        }
        println!("table blocks: {tables}, total blocks: {}", blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            if b.kind != 6 && b.text.contains('|') {
                println!(
                    "SUSPECT block {i}: kind={} text={:?}",
                    b.kind,
                    &b.text.chars().take(120).collect::<String>()
                );
            }
        }
    }

    #[test]
    fn debug_all_blocks() {
        let home = std::env::var("HOME").unwrap();
        let md = std::fs::read_to_string(format!("{home}/linqu_backport.md"))
            .unwrap();
        let blocks = blocks_from_markdown(&md, Path::new("."), true);
        for (i, b) in blocks.iter().enumerate() {
            let head: String = b.text.chars().take(70).collect();
            println!("{i:3} kind={} {:?}", b.kind, head.replace('\n', "\\n"));
        }
    }

    #[test]
    fn nested_code_blocks_in_lists_become_their_own_blocks() {
        let blocks = blocks_from_markdown(
            "3. Fill x\n   ```c\n   int f();\n   ```\n   after\n4. next\n",
            Path::new("."),
            true,
        );
        let kinds: Vec<_> = blocks.iter().map(|block| block.kind).collect();
        assert_eq!(kinds, [KIND_LIST, KIND_CODE, KIND_LIST, KIND_LIST]);
        assert_eq!(blocks[0].text, "3. Fill x");
        assert_eq!(blocks[1].text, "int f();\n");
        assert_eq!(blocks[2].text, "after");
        assert_eq!(blocks[3].text, "4. next");
    }

    #[test]
    fn debug_real_table() {
        let md = "\
## Result summary

| status | count |
|---|---|
| picked | 1531 |
| skip-x86-resctrl-cluster | 102 |

- after
";
        let blocks = blocks_from_markdown(md, Path::new("."), true);
        for b in &blocks {
            println!("kind={} text={:?}", b.kind, b.text);
        }
    }
}
