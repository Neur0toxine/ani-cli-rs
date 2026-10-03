//! Tolerant provider subtitle parsing and conversion to libass-compatible ASS.
//!
//! Cues are delimited by timestamps rather than blank lines: some providers put
//! blank paragraphs inside signs. A strict WebVTT demuxer stops at those paragraphs.
//! This deliberately implements a subset of WebVTT layout/CSS, not a browser engine.
use std::{fmt::Write, sync::LazyLock, time::Duration};

use futures_util::StreamExt;
use regex::Regex;
use reqwest::{Client, header};

use crate::{AniError, RequestHeaders, Result};

pub(crate) const MAX_SUBTITLE_BYTES: usize = 16 * 1024 * 1024;
const WIDTH: f64 = 1280.0;
const HEIGHT: f64 = 720.0;
const FONT_SIZE: f64 = 36.0;
const ASS_HEADER: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1280\nPlayResY: 720\nWrapStyle: 0\nScaledBorderAndShadow: yes\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,36,&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,32,32,32,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";

pub(crate) async fn fetch(client: &Client, url: &str, headers: &RequestHeaders) -> Result<Vec<u8>> {
    let mut request = client.get(url).timeout(Duration::from_secs(20));
    if let Some(value) = &headers.referer {
        request = request.header(header::REFERER, value);
    }
    if let Some(value) = &headers.origin {
        request = request.header(header::ORIGIN, value);
    }
    for (name, value) in &headers.extra {
        request = request.header(name, value);
    }
    let response = request.send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_SUBTITLE_BYTES as u64)
    {
        return Err(AniError::Provider(
            "subtitle exceeds the 16 MiB limit".into(),
        ));
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX_SUBTITLE_BYTES {
            return Err(AniError::Provider(
                "subtitle exceeds the 16 MiB limit".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Existing ASS/SSA stays intact, including styles and positioning. Other text
/// formats are converted without invoking an external process or writing files.
pub(crate) fn to_ass(bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(AniError::Provider(
            "subtitle exceeds the 16 MiB limit".into(),
        ));
    }
    let body = std::str::from_utf8(bytes)
        .map_err(|_| AniError::Provider("subtitle is not UTF-8".into()))?
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if body.trim_start().starts_with("[Script Info]") && body.contains("[Events]") {
        return Ok(body);
    }
    let webvtt = body.trim_start().starts_with("WEBVTT");
    let lines: Vec<_> = body.lines().collect();
    let rules = css_rules(&lines);
    let mut output = String::from(ASS_HEADER);
    let mut count = 0;
    let mut i = 0;
    while i < lines.len() {
        if metadata(lines[i]) {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            continue;
        }
        let Some((start, end, settings)) = timing(lines[i]) else {
            i += 1;
            continue;
        };
        let begin = i + 1;
        i = begin;
        while i < lines.len() && !lines[i].contains("-->") && !metadata(lines[i]) {
            i += 1;
        }
        let mut finish = i;
        // A cue identifier immediately preceding a timing line is not payload.
        if i < lines.len()
            && lines[i].contains("-->")
            && finish > begin
            && !lines[finish - 1].trim().is_empty()
            && finish >= begin + 2
            && lines[finish - 2].trim().is_empty()
        {
            // Blank paragraphs inside an open tag belong to the current sign,
            // even if the final paragraph immediately precedes the next cue.
            let candidate = lines[begin..finish - 1].join("\n");
            if !has_open_tag(&candidate) {
                finish -= 1;
            }
        }
        let payload = lines[begin..finish]
            .iter()
            .filter(|line| !line.trim().is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if end <= start || payload.is_empty() {
            continue;
        }
        let layout = layout(settings);
        let text = render_text(&payload, &rules, webvtt)?;
        let _ = writeln!(
            output,
            "Dialogue: 0,{},{},Default,,{},{},0,,{}{}",
            ass_time(start),
            ass_time(end),
            layout.margin_l,
            layout.margin_r,
            layout.tags,
            text
        );
        count += 1;
        if output.len() > MAX_SUBTITLE_BYTES {
            return Err(AniError::Provider(
                "converted subtitle exceeds the 16 MiB limit".into(),
            ));
        }
    }
    if count == 0 {
        return Err(AniError::Provider(
            "subtitle contains no supported cues".into(),
        ));
    }
    Ok(output)
}

fn metadata(line: &str) -> bool {
    let line = line.trim();
    ["WEBVTT", "STYLE", "REGION", "NOTE"].iter().any(|prefix| {
        line == *prefix
            || line
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with([' ', '\t']))
    })
}

fn timestamp(value: &str) -> Option<u64> {
    let value = value.replace(',', ".");
    let (clock, fraction) = value.split_once('.')?;
    if fraction.len() != 3 || !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parts = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let (h, m, s) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    if m >= 60 || s >= 60 || h > 999 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * 1000 + fraction.parse::<u64>().ok()?)
}

fn timing(line: &str) -> Option<(u64, u64, &str)> {
    let (start, rest) = line.split_once("-->")?;
    let rest = rest.trim();
    let end_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some((
        timestamp(start.trim())?,
        timestamp(&rest[..end_len])?,
        rest[end_len..].trim(),
    ))
}

fn ass_time(ms: u64) -> String {
    let cs = (ms + 5) / 10;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360_000,
        cs / 6_000 % 60,
        cs / 100 % 60,
        cs % 100
    )
}

#[derive(Default)]
struct Layout {
    tags: String,
    margin_l: u32,
    margin_r: u32,
}

fn percent(value: &str) -> Option<f64> {
    let n: f64 = value.strip_suffix('%')?.parse().ok()?;
    (n.is_finite() && (0.0..=100.0).contains(&n)).then_some(n)
}

fn layout(settings: &str) -> Layout {
    let fields: Vec<_> = settings
        .split_whitespace()
        .filter_map(|field| field.split_once(':'))
        .collect();
    let get = |key| {
        fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    };
    let align = match get("align").unwrap_or("center") {
        "start" | "left" => 1,
        "end" | "right" => 3,
        _ => 2,
    };
    let mut anchor_x = align;
    let mut x = match align {
        1 => 32.0,
        3 => WIDTH - 32.0,
        _ => WIDTH / 2.0,
    };
    if let Some(position) = get("position") {
        let (value, anchor) = position.split_once(',').unwrap_or((position, "auto"));
        if let Some(value) = percent(value) {
            x = value * WIDTH / 100.0;
        }
        anchor_x = match anchor {
            "line-left" => 1,
            "line-right" => 3,
            "center" => 2,
            _ => align,
        };
    }
    let mut y = HEIGHT - 32.0;
    let mut anchor_y = 0;
    if let Some(line) = get("line") {
        let (value, anchor) = line.split_once(',').unwrap_or((line, "start"));
        if let Some(value) = percent(value) {
            y = value * HEIGHT / 100.0;
            anchor_y = match anchor {
                "center" => 3,
                "end" => 0,
                _ => 6,
            };
        } else if let Ok(n) = value.parse::<i32>() {
            if n >= 0 {
                y = 32.0 + f64::from(n) * FONT_SIZE * 1.2;
                anchor_y = 6;
            } else {
                y = HEIGHT - 32.0 + f64::from(n + 1) * FONT_SIZE * 1.2;
            }
            y = y.clamp(0.0, HEIGHT);
        }
    }
    let mut result = Layout::default();
    if get("line").is_some() || get("position").is_some() || get("align").is_some() {
        result.tags = format!("{{\\an{}\\pos({x:.1},{y:.1})}}", anchor_y + anchor_x);
    }
    // Limit the cue box using ASS margins. Exact browser wrapping and vertical
    // writing/regions cannot be represented by this converter.
    if let Some(size) = get("size").and_then(percent) {
        let width = WIDTH * size / 100.0;
        let left = match anchor_x {
            1 => x,
            3 => x - width,
            _ => x - width / 2.0,
        }
        .clamp(0.0, WIDTH);
        result.margin_l = left as u32;
        result.margin_r = (WIDTH - left - width).max(0.0) as u32;
    }
    result
}

#[derive(Clone, Debug)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    color: [u8; 4],
    background: Option<[u8; 4]>,
    font: String,
    size: f64,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            color: [255; 4],
            background: None,
            font: "Arial".into(),
            size: FONT_SIZE,
        }
    }
}
impl Style {
    fn tags(&self) -> String {
        let [r, g, b, a] = self.color;
        let mut tags = format!(
            "{{\\b{}\\i{}\\u{}\\1c&H{b:02X}{g:02X}{r:02X}&\\1a&H{:02X}&\\fn{}\\fs{:.1}",
            u8::from(self.bold),
            u8::from(self.italic),
            u8::from(self.underline),
            255 - a,
            self.font,
            self.size
        );
        if let Some([r, g, b, a]) = self.background {
            let _ = write!(
                tags,
                "\\3c&H{b:02X}{g:02X}{r:02X}&\\3a&H{:02X}&\\bord4",
                255 - a
            );
        } else {
            tags.push_str("\\3c&H000000&\\3a&H00&\\bord2");
        }
        tags.push('}');
        tags
    }
    fn apply(&mut self, declarations: &str) {
        for declaration in declarations.split(';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "color" => {
                    if let Some(color) = color(value) {
                        self.color = color;
                    }
                }
                "background-color" => self.background = color(value),
                "font-weight" => {
                    self.bold = value == "bold" || value.parse::<u16>().is_ok_and(|n| n >= 600)
                }
                "font-style" => self.italic = matches!(value, "italic" | "oblique"),
                "text-decoration" => self.underline = value.contains("underline"),
                "font-family" => {
                    let font = value
                        .split(',')
                        .next()
                        .unwrap_or(value)
                        .trim()
                        .trim_matches(['\'', '"']);
                    if !font.is_empty()
                        && !font
                            .chars()
                            .any(|c| c.is_control() || matches!(c, '{' | '}' | '\\'))
                    {
                        self.font = font.into();
                    }
                }
                "font-size" => {
                    let size = if let Some(p) = value.strip_suffix('%') {
                        p.parse::<f64>().ok().map(|p| FONT_SIZE * p / 100.0)
                    } else {
                        value
                            .strip_suffix("px")
                            .unwrap_or(value)
                            .parse::<f64>()
                            .ok()
                    };
                    if let Some(size) = size.filter(|n| n.is_finite() && *n > 0.0 && *n <= HEIGHT) {
                        self.size = size;
                    }
                }
                _ => {}
            }
        }
    }
}
fn color(value: &str) -> Option<[u8; 4]> {
    value
        .parse::<csscolorparser::Color>()
        .ok()
        .map(|c| c.to_rgba8())
}

struct CssRule {
    selector: String,
    declarations: String,
}
static CSS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)::cue(?:\(([^)]*)\))?\s*\{([^}]*)\}").unwrap());
static ATTRIBUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap());
fn css_rules(lines: &[&str]) -> Vec<CssRule> {
    let mut css = String::new();
    let mut in_style = false;
    for line in lines {
        if line.trim() == "STYLE" {
            in_style = true;
            continue;
        }
        if in_style && line.trim().is_empty() {
            in_style = false;
        }
        if in_style {
            css.push_str(line);
            css.push('\n');
        }
    }
    CSS.captures_iter(&css)
        .map(|c| CssRule {
            selector: c.get(1).map_or("", |m| m.as_str()).trim().into(),
            declarations: c[2].into(),
        })
        .collect()
}
fn matching(selector: &str, tag: &str, classes: &[&str]) -> bool {
    // Simple tag/class selectors; unsupported selectors never match accidentally.
    let mut parts = selector.split('.');
    let name = parts.next().unwrap_or_default();
    (name.is_empty() || name == tag)
        && parts.all(|class| classes.contains(&class))
        && !selector.is_empty()
}

fn has_open_tag(payload: &str) -> bool {
    // Used only to distinguish cue IDs from paragraphs in malformed signs.
    let mut depth = 0i32;
    for part in payload.split('<').skip(1) {
        let Some((tag, _)) = part.split_once('>') else {
            continue;
        };
        if tag.starts_with('/') {
            depth -= 1;
        } else if !tag.starts_with(|c: char| c.is_ascii_digit()) && tag != "br" {
            depth += 1;
        }
    }
    depth > 0
}

fn escape_text(text: &str) -> String {
    html_escape::decode_html_entities(text)
        .chars()
        .map(|c| match c {
            '\\' => "\\\u{2060}".into(),
            '{' => "\\{{}".into(),
            '\n' => "\\N".into(),
            '\u{00a0}' => "\\h".into(),
            c if c.is_control() => " ".into(),
            c => c.to_string(),
        })
        .collect()
}

fn render_text(payload: &str, rules: &[CssRule], webvtt: bool) -> Result<String> {
    let mut current = Style::default();
    for rule in rules.iter().filter(|r| r.selector.is_empty()) {
        current.apply(&rule.declarations);
    }
    let mut output = current.tags();
    let mut stack: Vec<(String, Style)> = Vec::new();
    let mut rest = payload;
    while let Some(index) = rest.find('<') {
        if output.len() > MAX_SUBTITLE_BYTES {
            return Err(AniError::Provider(
                "converted cue exceeds the 16 MiB limit".into(),
            ));
        }
        output.push_str(&escape_text(&rest[..index]));
        let Some(end) = rest[index..].find('>') else {
            output.push_str(&escape_text(&rest[index..]));
            return Ok(output);
        };
        let token = &rest[index + 1..index + end];
        rest = &rest[index + end + 1..];
        if let Some(close) = token.strip_prefix('/') {
            if let Some(index) = stack.iter().rposition(|(tag, _)| tag == close.trim()) {
                current = stack[index].1.clone();
                stack.truncate(index);
                output.push_str(&current.tags());
            }
            continue;
        }
        let head = token
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches('/');
        let mut names = head.split('.');
        let tag = names.next().unwrap_or_default();
        let classes: Vec<_> = names.collect();
        if tag == "br" {
            output.push_str("\\N");
            continue;
        }
        if !matches!(
            tag,
            "b" | "i" | "u" | "c" | "font" | "v" | "lang" | "ruby" | "rt"
        ) {
            // Timestamp/unknown markup has no executable ASS interpretation.
            continue;
        }
        if stack.len() >= 64 {
            return Err(AniError::Provider(
                "subtitle markup is nested too deeply".into(),
            ));
        }
        stack.push((tag.into(), current.clone()));
        match tag {
            "b" => current.bold = true,
            "i" => current.italic = true,
            "u" => current.underline = true,
            _ => {}
        }
        if webvtt {
            for class in &classes {
                let background = class.strip_prefix("bg_");
                let name = background.unwrap_or(class);
                if matches!(
                    name,
                    "white" | "lime" | "cyan" | "red" | "yellow" | "magenta" | "blue" | "black"
                ) {
                    if background.is_some() {
                        current.background = color(name);
                    } else if let Some(color) = color(name) {
                        current.color = color;
                    }
                }
            }
        }
        if tag == "font" {
            for attr in ATTRIBUTE.captures_iter(token) {
                let value = attr
                    .get(2)
                    .or_else(|| attr.get(3))
                    .or_else(|| attr.get(4))
                    .unwrap()
                    .as_str();
                if &attr[1] == "color"
                    && let Some(color) = color(value)
                {
                    current.color = color;
                }
            }
        }
        for rule in rules
            .iter()
            .filter(|r| matching(&r.selector, tag, &classes))
        {
            current.apply(&rule.declarations);
        }
        output.push_str(&current.tags());
    }
    output.push_str(&escape_text(rest));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_blank_paragraphs_in_signs_and_keeps_later_dialogue() {
        let source = b"WEBVTT\n\n16:29.720 --> 16:34.930\n<b>Banish evil\n\n\nBegone\n\n\nCleanse</b>\n\n16:29.720 --> 16:34.930\n<b>Begone</b>\n\n16:35.580 --> 16:36.660\nIt's done!\n\n23:34.990 --> 23:40.250\nNext Time\n";
        let ass = to_ass(source).unwrap();
        assert_eq!(ass.matches("Dialogue:").count(), 4);
        assert!(ass.contains("Banish evil\\NBegone\\NCleanse"));
        assert!(ass.contains("0:16:35.58,0:16:36.66"));
        assert!(ass.contains("It's done!"));
        assert!(ass.contains("0:23:34.99,0:23:40.25"));
    }

    #[test]
    fn colors_nested_styles_and_position_survive_conversion() {
        let ass = to_ass(b"WEBVTT\n\nSTYLE\n::cue(.sign) { color: rgba(255, 0, 0, 0.5); font-weight: bold; }\n\n00:01.000 --> 00:03.000 line:10% position:20%,line-left align:start\n<c.sign>Sign <i>italic</i> red</c> white\n").unwrap();
        assert!(ass.contains("\\an7\\pos(256.0,72.0)"));
        assert!(ass.contains("\\1c&H0000FF&\\1a&H7F&"));
        assert!(ass.contains("\\b1\\i1"));
        assert!(ass.contains("\\b1\\i0"));
        assert!(ass.contains("\\b0\\i0\\u0\\1c&HFFFFFF&"));
    }

    #[test]
    fn skips_metadata_bad_cues_and_ids_without_losing_following_cues() {
        let ass = to_ass(b"\xef\xbb\xbfWEBVTT\r\n\r\nNOTE comment\r\n01:00.000 --> 01:01.000\r\nignore\r\n\r\nfirst-id\r\n00:01.000 --> 00:02.000\r\nFirst\r\n\r\nbad --> timestamp\r\nInvalid\r\n\r\nsecond-id\r\n00:04.000 --> 00:05.000\r\nSecond\r\n").unwrap();
        assert_eq!(ass.matches("Dialogue:").count(), 2);
        assert!(!ass.contains("second-id"));
        assert!(!ass.contains("ignore"));
        assert!(!ass.contains("Invalid"));
        assert!(ass.contains("Second"));
    }

    #[test]
    fn srt_entities_and_literal_ass_markup_are_safe() {
        let ass = to_ass(b"1\n00:00:01,000 --> 00:00:02,000\n<font color='#00ff00'>A &amp; B</font> {\\pos(0,0)}\n\n2\n00:00:03,000 --> 00:00:04,000\n&#26085;&#26412; &lt;\n").unwrap();
        assert_eq!(ass.matches("Dialogue:").count(), 2);
        assert!(ass.contains("\\1c&H00FF00&"));
        assert!(ass.contains("A & B"));
        assert!(ass.contains("日本 <"));
        assert!(!ass.contains("{\\pos(0,0)}"));
    }

    #[test]
    fn native_ass_is_preserved_and_invalid_files_rejected() {
        let source =
            "[Script Info]\nScriptType: v4.00+\n[Events]\nDialogue: custom styles and positions";
        assert_eq!(to_ass(source.as_bytes()).unwrap(), source);
        assert!(to_ass(b"<html>error page</html>").is_err());
        assert!(to_ass(&[0xff]).is_err());
    }

    #[test]
    fn default_colors_background_fonts_and_cue_boxes_are_supported() {
        let ass = to_ass(b"WEBVTT\n\nSTYLE\n::cue { font-size: 150%; font-family: 'Noto Sans'; }\n\n00:01.000 --> 00:02.000 line:0 position:10%,line-left size:60%\n<c.lime.bg_blue>Colored</c> plain\n").unwrap();
        assert!(ass.contains("\\fnNoto Sans\\fs54.0"));
        assert!(ass.contains("\\1c&H00FF00&"));
        assert!(ass.contains("\\3c&HFF0000&"));
        assert!(ass.contains("\\an7\\pos(128.0,32.0)"));
        assert!(ass.contains(",Default,,128,384,0,,"));
    }
}
