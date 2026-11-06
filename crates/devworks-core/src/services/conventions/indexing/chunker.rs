//! Retained MarkdownWikiChunker semantics (CodeGraph afe73560), without a document-sized
//! word/chunk array. One bounded source is owned at a time; the overlap window is 512 words.
use std::collections::{BTreeMap, VecDeque};

pub const WORDS: usize = 512;
pub const OVERLAP: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownChunk {
    pub index: u64,
    pub section_path: String,
    pub text: String,
    pub embedding_text: String,
}
struct Section {
    text: String,
    path: String,
    offset: usize,
    window: VecDeque<String>,
    short: bool,
    done: bool,
}
impl Section {
    fn new(text: &str, path: String) -> Self {
        Self {
            text: text.to_owned(),
            path,
            offset: 0,
            window: VecDeque::new(),
            short: text.split_whitespace().take(WORDS + 1).count() <= WORDS,
            done: false,
        }
    }
    fn part(&mut self) -> Option<String> {
        if self.done {
            return None;
        }
        if self.short {
            self.done = true;
            return Some(self.text.clone());
        }
        while self.window.len() < WORDS {
            let remaining = &self.text[self.offset..];
            let lead = remaining.len() - remaining.trim_start().len();
            self.offset += lead;
            if self.offset == self.text.len() {
                break;
            }
            let end = self.text[self.offset..]
                .find(char::is_whitespace)
                .map_or(self.text.len(), |end| self.offset + end);
            self.window
                .push_back(self.text[self.offset..end].to_owned());
            self.offset = end;
        }
        if self.window.is_empty() {
            self.done = true;
            return None;
        }
        let part = self
            .window
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        if self.text[self.offset..].trim_start().is_empty() {
            self.done = true;
        } else {
            while self.window.len() > OVERLAP {
                self.window.pop_front();
            }
        }
        Some(part)
    }
}

/// A streaming chunk iterator over one source-legal Markdown body. No source text is
/// truncated: a single long word or heading can exceed an ordinary display-field bound.
pub struct MarkdownChunks {
    source: String,
    path: String,
    scan: usize,
    section_start: usize,
    breadcrumb: BTreeMap<usize, String>,
    section_breadcrumb: Vec<String>,
    current: Option<Section>,
    ended: bool,
    index: u64,
}
impl MarkdownChunks {
    pub fn new(markdown: &str, section_path: &str) -> Self {
        let source = markdown.replace("\r\n", "\n");
        let start = if let Some(rest) = source.strip_prefix("---") {
            rest.find("\n---")
                .map(|end| end + 3)
                .map(|end| {
                    source[end + 4..]
                        .find('\n')
                        .map_or(source.len(), |next| end + 4 + next + 1)
                })
                .unwrap_or(0)
        } else {
            0
        };
        Self {
            source,
            path: section_path.to_owned(),
            scan: start,
            section_start: start,
            breadcrumb: BTreeMap::new(),
            section_breadcrumb: Vec::new(),
            current: None,
            ended: false,
            index: 0,
        }
    }
    fn section(&mut self) -> Option<Section> {
        while !self.ended {
            let mut end = self.source.len();
            let mut next = None;
            while self.scan < self.source.len() {
                let start = self.scan;
                let line_end = self.source[start..]
                    .find('\n')
                    .map_or(self.source.len(), |n| start + n);
                self.scan = if line_end < self.source.len() {
                    line_end + 1
                } else {
                    line_end
                };
                if let Some((level, title)) = heading(&self.source[start..line_end]) {
                    end = start;
                    next = Some((level, title.to_owned(), self.scan));
                    break;
                }
            }
            let text = self.source[self.section_start..end].trim();
            let path = if self.section_breadcrumb.is_empty() {
                self.path.clone()
            } else {
                format!("{} > {}", self.path, self.section_breadcrumb.join(" > "))
            };
            let section = (!text.is_empty() && !pure_code(text)).then(|| Section::new(text, path));
            if let Some((level, title, start)) = next {
                self.breadcrumb.retain(|key, _| *key <= level);
                self.breadcrumb
                    .insert(level, format!("{} {title}", "#".repeat(level)));
                self.section_breadcrumb = self.breadcrumb.values().cloned().collect();
                self.section_start = start;
            } else {
                self.ended = true;
            }
            if section.is_some() {
                return section;
            }
        }
        None
    }
}
impl Iterator for MarkdownChunks {
    type Item = MarkdownChunk;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(section) = &mut self.current
                && let Some(text) = section.part()
            {
                let text = text.trim().to_owned();
                if text.is_empty() {
                    continue;
                }
                let index = self.index;
                self.index += 1;
                return Some(MarkdownChunk {
                    index,
                    section_path: section.path.clone(),
                    embedding_text: format!("{}\n\n{text}", section.path),
                    text,
                });
            }
            self.current = self.section();
            self.current.as_ref()?;
        }
    }
}
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=3).contains(&level) || !line[level..].starts_with(char::is_whitespace) {
        return None;
    }
    let text = line[level..].trim();
    (!text.is_empty()).then_some((level, text))
}
fn pure_code(text: &str) -> bool {
    text.starts_with("```")
        && text
            .rfind("```")
            .is_some_and(|last| last > 0 && last >= text.len() - 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_breadcrumb_frontmatter_and_code_exclusion_are_preserved() {
        let input = "---\r\nowner: platform\r\n---\r\n# Overview\r\n\r\nIntro text.\r\n\r\n## Consumers\r\n\r\nPrefer idempotent consumers.\r\n### Code\r\n```rust\r\nlet n = 1;\r\n```\r\n## Next\r\n#### Keep this lower heading\r\nText.";
        let chunks: Vec<_> = MarkdownChunks::new(input, "Messaging").collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0].embedding_text,
            "Messaging > # Overview\n\nIntro text."
        );
        assert_eq!(
            chunks[1].section_path,
            "Messaging > # Overview > ## Consumers"
        );
        assert_eq!(chunks[1].index, 1);
        assert!(chunks[2].text.starts_with("#### Keep this lower heading"));
        assert!(
            !chunks
                .iter()
                .any(|c| c.text.contains("owner:") || c.text.contains("let n"))
        );
        assert!(
            MarkdownChunks::new("---\nkey: value\n---", "P")
                .next()
                .is_none()
        );
    }
    #[test]
    fn long_sections_have_exact_512_word_windows_and_64_word_overlap() {
        let source = (0..1025)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks: Vec<_> = MarkdownChunks::new(&source, "P").collect();
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.text.split_whitespace().count())
                .collect::<Vec<_>>(),
            [512, 512, 129]
        );
        assert!(chunks[0].text.ends_with("word511"));
        assert!(chunks[1].text.starts_with("word448 "));
        assert!(chunks[2].text.starts_with("word896 "));
        assert!(chunks[2].text.ends_with("word1024"));
        assert_eq!(
            MarkdownChunks::new("line one\n\nline two", "P")
                .next()
                .unwrap()
                .text,
            "line one\n\nline two"
        );
    }
    #[test]
    fn legal_long_words_and_headings_are_not_narrowed_to_display_bounds() {
        let heading = "h".repeat(70_000);
        let word = "w".repeat(1_000_000);
        let chunk = MarkdownChunks::new(&format!("# {heading}\n{word}"), "P")
            .next()
            .unwrap();
        assert_eq!(chunk.text, word);
        assert_eq!(chunk.section_path, format!("P > # {heading}"));
        assert!(chunk.embedding_text.ends_with(&word));
        assert_eq!(
            MarkdownChunks::new(&"x".repeat(16_777_215), "P")
                .next()
                .unwrap()
                .text
                .len(),
            16_777_215
        );
    }
}
