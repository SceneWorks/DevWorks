//! Word stores some links as field codes. Read the code as data and keep the cached body text.
use super::{WordLink, WordLinkTarget};

pub(super) const MAX_CODE_BYTES: usize = 4096;

pub(super) fn hyperlink(code: &str) -> Result<Option<WordLink>, ()> {
    let Some((name, mut rest)) = token(code)? else {
        return Ok(None);
    };
    if !name.eq_ignore_ascii_case("HYPERLINK") {
        return Ok(None);
    }
    let mut target = None;
    let mut bookmark = None;
    while let Some((part, tail)) = token(rest)? {
        rest = tail;
        if part.eq_ignore_ascii_case(r"\l") {
            let (anchor, tail) = token(rest)?.ok_or(())?;
            bookmark = Some(anchor.to_owned());
            rest = tail;
        } else if [r"\o", r"\t"]
            .iter()
            .any(|option| part.eq_ignore_ascii_case(option))
        {
            rest = token(rest)?.ok_or(())?.1;
        } else if !part.starts_with('\\') && target.is_none() && !part.is_empty() {
            target = Some(part.to_owned());
        }
    }
    let target = match (&bookmark, target) {
        (_, Some(target)) if target.starts_with('#') => {
            WordLinkTarget::Bookmark(target[1..].to_owned())
        }
        (_, Some(target)) => WordLinkTarget::External(target),
        (Some(anchor), None) => WordLinkTarget::Bookmark(anchor.clone()),
        (None, None) => return Err(()),
    };
    Ok(Some(WordLink {
        target,
        anchor: bookmark,
    }))
}

fn token(source: &str) -> Result<Option<(&str, &str)>, ()> {
    let source = source.trim_start();
    if source.is_empty() {
        return Ok(None);
    }
    if let Some(quoted) = source.strip_prefix('"') {
        let mut backslash = false;
        for (index, scalar) in quoted.char_indices() {
            if scalar == '"' && !backslash {
                return Ok(Some((&quoted[..index], &quoted[index + 1..])));
            }
            backslash = scalar == '\\' && !backslash;
        }
        return Err(());
    }
    let end = source.find(char::is_whitespace).unwrap_or(source.len());
    Ok(Some(source.split_at(end)))
}
