//! Content handling of fetched pages: which media types are kept, how bytes become text and how
//! HTML is reduced to readable text. Everything here is pure and bounded by its input.
//!
//! * HTML (`text/html`, `application/xhtml+xml`) is reduced to text: scripts, styles, templates,
//!   embedded SVG/MathML and comments are dropped, block elements become line breaks, list items
//!   bullets, character references are decoded and whitespace is collapsed (kept inside `pre`).
//! * Plain text and structured text (`text/*`, JSON, XML, YAML, TOML, JavaScript) pass through.
//! * Everything else (images, archives, PDFs, executables, unknown types) is refused before the
//!   body is read; a response without a media type is kept only when it is valid UTF-8 text.

/// How a fetched body is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentKind {
    Html,
    /// Text kept as received (plain, markup, code).
    Text,
    /// JSON kept as received.
    Json,
}

/// The parsed `Content-Type` of a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaType {
    /// Lowercase `type/subtype`, without parameters.
    pub essence: String,
    pub charset: Option<String>,
}

pub fn media_type(header: &str) -> MediaType {
    let mut parts = header.split(';');
    let essence = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let charset = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches('"').to_ascii_lowercase())
    });
    MediaType { essence, charset }
}

/// How a media type is kept, or `None` when it is refused.
pub fn kind(essence: &str) -> Option<ContentKind> {
    match essence {
        "text/html" | "application/xhtml+xml" => Some(ContentKind::Html),
        "application/json" | "application/ld+json" => Some(ContentKind::Json),
        e if e.ends_with("+json") => Some(ContentKind::Json),
        e if e.starts_with("text/") => Some(ContentKind::Text),
        "application/xml"
        | "application/javascript"
        | "application/x-javascript"
        | "application/ecmascript"
        | "application/yaml"
        | "application/x-yaml"
        | "application/toml"
        | "application/x-sh"
        | "application/sql"
        | "application/graphql" => Some(ContentKind::Text),
        e if e.ends_with("+xml") => Some(ContentKind::Text),
        _ => None,
    }
}

/// Decodes `bytes` with the declared charset (UTF-8, ASCII, ISO-8859-1, windows-1252; anything
/// else is read as UTF-8 with replacement characters). A byte-order mark is removed.
pub fn decode(bytes: &[u8], charset: Option<&str>) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match charset {
        Some("iso-8859-1" | "latin1" | "l1" | "iso_8859-1" | "iso8859-1") => {
            bytes.iter().map(|&b| char::from(b)).collect()
        }
        Some("windows-1252" | "cp1252" | "x-cp1252") => bytes.iter().map(|&b| cp1252(b)).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9f => HIGH[(b - 0x80) as usize],
        _ => char::from(b),
    }
}

/// The charset an HTML document declares in its first kilobyte (`<meta charset>` or
/// `http-equiv` content), used when the response header names none.
pub fn html_charset(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    let at = head.find("charset=")? + "charset=".len();
    let value: String = head[at..]
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Elements whose content is never text.
const SKIPPED: [&str; 7] = [
    "script", "style", "noscript", "template", "svg", "math", "iframe",
];
/// Elements that start or end a line.
const BLOCKS: [&str; 33] = [
    "p",
    "div",
    "br",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "table",
    "tr",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "aside",
    "main",
    "blockquote",
    "pre",
    "figure",
    "figcaption",
    "form",
    "fieldset",
    "details",
    "summary",
    "address",
];

/// Reduces HTML to text, returning it with the document title.
pub fn html_to_text(html: &str) -> (String, Option<String>) {
    let mut out = String::with_capacity(html.len() / 2);
    let mut title: Option<String> = None;
    let mut in_title = false;
    let mut title_text = String::new();
    let mut pre = 0usize;
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, rest, pre > 0, in_title.then_some(&mut title_text));
            break;
        };
        push_text(
            &mut out,
            &rest[..lt],
            pre > 0,
            in_title.then_some(&mut title_text),
        );
        rest = &rest[lt..];
        // `<` not followed by a tag name, `/`, `!` or `?` is text, as browsers read it.
        if !rest[1..].starts_with(|c: char| c.is_ascii_alphabetic() || "/!?".contains(c)) {
            push_text(&mut out, "<", pre > 0, in_title.then_some(&mut title_text));
            rest = &rest[1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |e| &after[e + 3..]);
            continue;
        }
        let Some(gt) = tag_end(rest) else {
            // An unterminated tag: the rest is not text.
            break;
        };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if name == "title" {
            if closing {
                if in_title && title.is_none() {
                    let t = collapse(&decode_entities(&title_text));
                    title = (!t.is_empty()).then_some(t);
                }
                in_title = false;
            } else {
                in_title = true;
                title_text.clear();
            }
            continue;
        }
        if !closing && SKIPPED.contains(&name.as_str()) && !tag.ends_with('/') {
            // Skip to the matching close tag (case-insensitive); nothing inside is text.
            rest = skip_element(rest, &name);
            continue;
        }
        if name == "pre" {
            pre = if closing {
                pre.saturating_sub(1)
            } else {
                pre + 1
            };
        }
        if name == "li" && !closing {
            newline(&mut out);
            out.push_str("- ");
        } else if name == "td" || name == "th" {
            if !closing && !out.ends_with(['\n', ' ', '\t']) && !out.is_empty() {
                out.push('\t');
            }
        } else if BLOCKS.contains(&name.as_str()) {
            newline(&mut out);
            if matches!(name.as_str(), "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") && closing {
                out.push('\n');
            }
        }
    }
    (tidy(&out), title)
}

/// The end of a tag starting at `s[0] == '<'`, skipping `>` inside quoted attribute values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// The text after the close tag of `name` (ASCII case-insensitive), or nothing when the element
/// never closes. Linear in the skipped text.
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let bytes = rest.as_bytes();
    let close = name.as_bytes();
    let mut i = 0;
    while let Some(n) = rest[i..].find("</") {
        let at = i + n + 2;
        let candidate = &bytes[at..(at + close.len()).min(bytes.len())];
        if candidate.eq_ignore_ascii_case(close) {
            let after = &rest[at..];
            return after.find('>').map_or("", |e| &after[e + 1..]);
        }
        i = at;
    }
    ""
}

fn newline(out: &mut String) {
    while out.ends_with([' ', '\t']) {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn push_text(out: &mut String, raw: &str, pre: bool, title: Option<&mut String>) {
    if let Some(t) = title {
        t.push_str(raw);
        return;
    }
    let text = decode_entities(raw);
    if pre {
        out.push_str(&text);
        return;
    }
    for c in text.chars() {
        if c.is_whitespace() {
            if !out.is_empty() && !out.ends_with([' ', '\n', '\t']) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Trims trailing spaces on every line and keeps at most one blank line in a row.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 || out.is_empty() {
                continue;
            }
            out.push('\n');
            continue;
        }
        blank = 0;
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Decodes character references: numeric ones and the common named ones.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map(|n| n + 1);
        let (name, consumed) = match end {
            Some(e) if rest[e..].starts_with(';') => (&rest[1..e], e + 1),
            Some(e) => (&rest[1..e], e),
            None => (&rest[1..], rest.len()),
        };
        match entity(name) {
            Some(c) if !name.is_empty() => {
                out.push(c);
                rest = &rest[consumed..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        // NUL and surrogates become the replacement character, as browsers do.
        return Some(
            char::from_u32(code)
                .filter(|c| *c != '\0')
                .unwrap_or('\u{fffd}'),
        );
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "times" => '×',
        "deg" => '°',
        "rarr" => '→',
        "larr" => '←',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text_without_scripts_or_styles() {
        let html = r#"<!DOCTYPE html><html><head><title> Rust &amp; Docs </title>
            <style>body{color:red}</style><script>alert("x > y")</script></head>
            <body><h1>Guide</h1><p>Hello&nbsp;<b>world</b>&#33; It&#x27;s &lt;fine&gt;.</p>
            <!-- hidden --><ul><li>one</li><li>two</li></ul>
            <pre>  keep
   spacing</pre><SCRIPT type="x">ignored()</SCRIPT><a href="/x" title="a>b">link</a></body></html>"#;
        let (text, title) = html_to_text(html);
        assert_eq!(title.as_deref(), Some("Rust & Docs"));
        assert!(text.contains("Guide\n"), "{text}");
        assert!(text.contains("Hello world! It's <fine>."), "{text}");
        assert!(text.contains("- one\n- two"), "{text}");
        assert!(text.contains("  keep\n   spacing"), "{text}");
        assert!(text.contains("link"), "{text}");
        for absent in ["alert", "color:red", "hidden", "ignored", "a>b"] {
            assert!(!text.contains(absent), "{absent} in {text}");
        }
    }

    #[test]
    fn media_types_are_kept_or_refused() {
        let m = media_type("Text/HTML; Charset=\"UTF-8\"");
        assert_eq!(m.essence, "text/html");
        assert_eq!(m.charset.as_deref(), Some("utf-8"));
        assert_eq!(kind("text/html"), Some(ContentKind::Html));
        assert_eq!(kind("text/plain"), Some(ContentKind::Text));
        assert_eq!(kind("text/markdown"), Some(ContentKind::Text));
        assert_eq!(kind("application/json"), Some(ContentKind::Json));
        assert_eq!(kind("application/vnd.api+json"), Some(ContentKind::Json));
        assert_eq!(kind("application/atom+xml"), Some(ContentKind::Text));
        for refused in [
            "application/pdf",
            "image/png",
            "application/octet-stream",
            "application/zip",
            "video/mp4",
            "",
        ] {
            assert_eq!(kind(refused), None, "{refused}");
        }
    }

    #[test]
    fn charsets_decode() {
        assert_eq!(decode(b"caf\xe9", Some("iso-8859-1")), "café");
        assert_eq!(decode(b"\x93q\x94", Some("windows-1252")), "“q”");
        assert_eq!(decode(b"\xef\xbb\xbfhi", None), "hi");
        assert_eq!(
            html_charset(br#"<meta charset="windows-1252">"#).as_deref(),
            Some("windows-1252")
        );
        assert_eq!(
            decode_entities("&bogus; &#0; &#xD800;"),
            "&bogus; \u{fffd} \u{fffd}"
        );
    }
}
