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
                    let trimmed = s.trim_start();
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
        match self.state {
            State::Leading => (String::new(), pending),
            State::Inside => (pending, String::new()),
            _ => (String::new(), String::new()),
        }
    }
}
