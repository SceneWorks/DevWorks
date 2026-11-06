//! Retained leading-tag state machine from simple-router a556b353 Local/ThinkTagFilter.cs.
//! Only a leading (after whitespace) block qualifies; mentions in ordinary prose stay text.
#[derive(Default)]
enum State {
    #[default]
    Leading,
    Inside,
    AfterClose,
    Passthrough,
}

#[derive(Default)]
pub(super) struct ThinkTagFilter {
    pending: String,
    // Byte boundary after whitespace already examined while retaining the exact prefix.
    leading_whitespace: usize,
    #[cfg(test)]
    leading_examined: usize,
    state: State,
}
impl ThinkTagFilter {
    pub(super) fn feed(&mut self, text: &str) -> (String, String) {
        const OPEN: &str = "<think>";
        const CLOSE: &str = "</think>";
        self.pending.push_str(text);
        let mut s = std::mem::take(&mut self.pending);
        let mut thinking = String::new();
        loop {
            match self.state {
                State::Leading => {
                    let unexamined = &s[self.leading_whitespace..];
                    #[cfg(test)]
                    {
                        self.leading_examined += unexamined.len();
                    }
                    let trimmed = unexamined.trim_start();
                    self.leading_whitespace = s.len() - trimmed.len();
                    if let Some(rest) = trimmed.strip_prefix(OPEN) {
                        s = rest.into();
                        self.state = State::Inside;
                        continue;
                    }
                    if OPEN.starts_with(trimmed) {
                        self.pending = s;
                        return (thinking, String::new());
                    }
                    self.state = State::Passthrough;
                    return (thinking, s);
                }
                State::Inside => {
                    if let Some(end) = s.find(CLOSE) {
                        thinking.push_str(&s[..end]);
                        s = s[end + CLOSE.len()..].into();
                        self.state = State::AfterClose;
                        continue;
                    }
                    let held = (1..CLOSE.len())
                        .rev()
                        .find(|n| s.ends_with(&CLOSE[..*n]))
                        .unwrap_or(0);
                    thinking.push_str(&s[..s.len() - held]);
                    self.pending = s[s.len() - held..].into();
                    return (thinking, String::new());
                }
                State::AfterClose => {
                    s = s.trim_start().into();
                    if !s.is_empty() {
                        self.state = State::Passthrough;
                    }
                    return (thinking, s);
                }
                State::Passthrough => return (thinking, s),
            }
        }
    }
    pub(super) fn flush(&mut self) -> (String, String) {
        let pending = std::mem::take(&mut self.pending);
        self.leading_whitespace = 0;
        match self.state {
            State::Leading => (String::new(), pending),
            State::Inside => (pending, String::new()),
            _ => (String::new(), String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leading_whitespace_is_examined_once_and_retained_for_flush_or_prose() {
        for fragment in [" ".repeat(512), "\u{2003}\t ".repeat(128)] {
            for ending in ["", "plain text", "<thi", "<think>reason</think> answer"] {
                let mut filter = ThinkTagFilter::default();
                for index in 0..8192 {
                    assert_eq!(filter.feed(&fragment), (String::new(), String::new()));
                    assert!(
                        filter.leading_examined <= fragment.len() * (index + 1),
                        "leading scan revisited old input: {} examined",
                        filter.leading_examined
                    );
                }
                let (thinking, visible) = filter.feed(ending);
                let (remaining_thinking, remaining_visible) = filter.flush();
                if ending.starts_with("<think>") {
                    assert_eq!(thinking, "reason");
                    assert_eq!(visible, "answer");
                    assert!(remaining_thinking.is_empty());
                    assert!(remaining_visible.is_empty());
                } else {
                    assert!(thinking.is_empty());
                    assert!(remaining_thinking.is_empty());
                    assert_eq!(
                        format!("{visible}{remaining_visible}"),
                        format!("{}{ending}", fragment.repeat(8192))
                    );
                }
                assert!(
                    filter.leading_examined <= fragment.len() * 8192 + ending.len(),
                    "leading scan revisited old input: {} examined",
                    filter.leading_examined
                );
            }
        }
    }
}
