//! Versioned opaque cursors for session and history pages (shared-operation-contracts.md
//! "Queries, cursors and event delivery").
//!
//! A cursor is `v1.<hex position>.<tag>`, where the tag is a digest of the binding and the
//! position. The binding names the operation and its query/filter, the invoking principal and
//! project (visibility), the store and restore epoch, and the scope revision the page was read
//! under. A cursor presented under any other binding — another query, principal, project, store,
//! restore epoch or a changed scope revision — or one that was altered is `CursorExpired`, so a
//! caller re-snapshots instead of silently continuing a different list.
//!
//! Every list pages through this module: [`page`] for an ordered store read and [`slice`] for a
//! list already in memory. Both apply one rule: a page reports `has_more` only when a row
//! exists after its last row, and only then carries a `next_cursor`. A cursor in any other or
//! older format is `CursorExpired`, so the client restarts the listing.
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{MAX_PAGE, StorageError};
use crate::protocol::records::PageRequest;
use crate::protocol::support::{BoundedList, BoundedText, Cursor, Page};

const VERSION: &str = "v1";

/// What a cursor is bound to.
#[derive(Clone, Debug)]
pub struct CursorBinding(String);
impl CursorBinding {
    /// Binds `operation` with its query/filter `parts` for `ctx` at `scope_revision`.
    pub fn new(
        ctx: &InvocationContext,
        operation: &str,
        parts: &[&str],
        scope_revision: i64,
    ) -> Self {
        let mut basis = format!(
            "{operation}|{}|{}|{}|{}|{scope_revision}",
            ctx.principal().0.as_str(),
            ctx.project_id().map_or("", |p| p.0.as_str()),
            ctx.store_id().0.as_str(),
            ctx.restore_epoch().as_str(),
        );
        for p in parts {
            basis.push('|');
            basis.push_str(p);
        }
        Self(sha256_hex(basis.as_bytes()))
    }
    fn tag(&self, position: &str) -> String {
        sha256_hex(format!("{}|{position}", self.0).as_bytes())[..24].to_owned()
    }
    /// The cursor resuming after `position`.
    pub fn encode(&self, position: &str) -> String {
        let hex: String = position.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{VERSION}.{hex}.{}", self.tag(position))
    }
    /// The wire cursor resuming after `position`.
    pub fn cursor(&self, position: &str) -> Cursor {
        BoundedText(self.encode(position))
    }
    /// The position of a cursor issued under this binding.
    pub fn decode(&self, cursor: &str) -> Result<String, StorageError> {
        let (position, tag) = split(cursor)?;
        if self.tag(&position) != tag {
            return Err(StorageError::CursorExpired);
        }
        Ok(position)
    }
    /// The position `request` resumes after, or `None` for a first page.
    pub fn resume(&self, request: &PageRequest) -> Result<Option<String>, StorageError> {
        request
            .cursor
            .as_ref()
            .map(|c| self.decode(&c.0))
            .transpose()
    }
}

fn split(cursor: &str) -> Result<(String, &str), StorageError> {
    let mut parts = cursor.split('.');
    let (Some(VERSION), Some(hex), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(StorageError::CursorExpired);
    };
    if !hex.is_ascii() || hex.len() % 2 != 0 {
        return Err(StorageError::CursorExpired);
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| StorageError::CursorExpired)?;
    let position = String::from_utf8(bytes).map_err(|_| StorageError::CursorExpired)?;
    Ok((position, tag))
}

/// The position text of a cursor before its binding is checked. Only for a listing whose
/// binding depends on what the cursor carries (a date range the first page resolved); the
/// caller must still [`CursorBinding::decode`] the cursor under the binding it then builds.
pub fn peek(cursor: &str) -> Result<String, StorageError> {
    split(cursor).map(|(position, _)| position)
}

/// The rows of one page, before they become wire records.
#[derive(Debug)]
pub struct Paged<T> {
    pub items: Vec<T>,
    /// Present exactly when a row follows the last item.
    pub next_cursor: Option<Cursor>,
}
impl<T> Paged<T> {
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }
    /// The wire page, mapping each row.
    pub fn try_into_page<U, E>(self, f: impl FnMut(T) -> Result<U, E>) -> Result<Page<U>, E> {
        Ok(Page {
            has_more: self.has_more(),
            items: BoundedList(self.items.into_iter().map(f).collect::<Result<_, _>>()?),
            next_cursor: self.next_cursor,
        })
    }
    pub fn into_page(self) -> Page<T> {
        Page {
            has_more: self.has_more(),
            items: BoundedList(self.items),
            next_cursor: self.next_cursor,
        }
    }
    /// The same page with each row mapped.
    pub fn try_map<U, E>(self, f: impl FnMut(T) -> Result<U, E>) -> Result<Paged<U>, E> {
        Ok(Paged {
            items: self.items.into_iter().map(f).collect::<Result<_, _>>()?,
            next_cursor: self.next_cursor,
        })
    }
    /// Keeps the leading rows whose JSON fits in `budget` bytes. A page cut short continues
    /// after its new last row; a first row that alone exceeds the budget is an error.
    pub fn clip_to_frame(
        mut self,
        binding: &CursorBinding,
        budget: usize,
        position_of: impl Fn(&T) -> String,
    ) -> Result<Self, StorageError>
    where
        T: serde::Serialize,
    {
        let mut bytes = 0usize;
        let mut kept = 0;
        for row in &self.items {
            let size = serde_json::to_vec(row)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
                .len()
                + 1;
            if bytes + size > budget {
                break;
            }
            bytes += size;
            kept += 1;
        }
        if kept == 0 && !self.items.is_empty() {
            return Err(StorageError::InvalidRecord(
                "a row exceeds the response bound".into(),
            ));
        }
        if kept < self.items.len() {
            self.items.truncate(kept);
            self.next_cursor = self
                .items
                .last()
                .map(|last| binding.cursor(&position_of(last)));
        }
        Ok(self)
    }
}

/// The page size `request` asks for, at most [`MAX_PAGE`].
pub fn limit(request: &PageRequest) -> u16 {
    u16::try_from(request.limit.get()).map_or(MAX_PAGE, |l| l.min(MAX_PAGE))
}

/// One page from an ordered store read.
///
/// `fetch(after, n)` returns at most `n` rows in listing order after the position `after`
/// (`None`: from the start) and maps a position it cannot parse to `CursorExpired`.
/// `position_of` names a row's position. The helper reads one row beyond the page to learn
/// whether more follow; at [`MAX_PAGE`], the store's bound, it reads one row after the last
/// instead. An error from either read is returned, never taken as the end of the list.
pub fn page<T, E: From<StorageError>>(
    binding: &CursorBinding,
    request: &PageRequest,
    mut fetch: impl FnMut(Option<&str>, u16) -> Result<Vec<T>, E>,
    position_of: impl Fn(&T) -> String,
) -> Result<Paged<T>, E> {
    let after = binding.resume(request)?;
    let limit = limit(request);
    let mut items = fetch(after.as_deref(), limit.saturating_add(1).min(MAX_PAGE))?;
    let more = if items.len() > usize::from(limit) {
        items.truncate(usize::from(limit));
        true
    } else {
        match items.last() {
            Some(last) if items.len() == usize::from(limit) => {
                !fetch(Some(&position_of(last)), 1)?.is_empty()
            }
            _ => false,
        }
    };
    let next_cursor = items
        .last()
        .filter(|_| more)
        .map(|last| binding.cursor(&position_of(last)));
    Ok(Paged { items, next_cursor })
}

/// Up to `n` rows that `keep` accepts, read in [`MAX_PAGE`] batches after `after` until `n` are
/// kept or the store has no more: the `fetch` of a [`page`] whose store read cannot apply the
/// whole filter. `position_of` names the position the next batch starts after.
pub fn scan<T, P, E>(
    mut after: P,
    n: u16,
    mut read: impl FnMut(&P, u16) -> Result<Vec<T>, E>,
    position_of: impl Fn(&T) -> P,
    mut keep: impl FnMut(&T) -> Result<bool, E>,
) -> Result<Vec<T>, E> {
    let mut kept = Vec::new();
    loop {
        let batch = read(&after, MAX_PAGE)?;
        let full = batch.len() >= usize::from(MAX_PAGE);
        for row in batch {
            after = position_of(&row);
            if keep(&row)? {
                kept.push(row);
                if kept.len() >= usize::from(n) {
                    return Ok(kept);
                }
            }
        }
        if !full {
            return Ok(kept);
        }
    }
}

/// One page of a list already in memory, by offset. The binding must name everything that
/// decides the list's contents and order, so an offset is never applied to another list.
pub fn slice<T>(
    binding: &CursorBinding,
    items: Vec<T>,
    request: &PageRequest,
) -> Result<Page<T>, StorageError> {
    let start = match binding.resume(request)? {
        None => 0,
        Some(at) => at
            .parse::<usize>()
            .map_err(|_| StorageError::CursorExpired)?,
    };
    if start > items.len() {
        return Err(StorageError::CursorExpired);
    }
    let end = start
        .saturating_add(usize::from(limit(request)))
        .min(items.len());
    let more = end < items.len();
    Ok(Page {
        items: BoundedList(items.into_iter().skip(start).take(end - start).collect()),
        next_cursor: more.then(|| binding.cursor(&end.to_string())),
        has_more: more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(limit: u32, cursor: Option<Cursor>) -> PageRequest {
        PageRequest {
            limit: crate::protocol::support::Limit::new(limit).unwrap(),
            cursor,
        }
    }
    /// A store read over `0..len` that bounds every read at MAX_PAGE, as a port does.
    fn fetch(len: i64) -> impl FnMut(Option<&str>, u16) -> Result<Vec<i64>, StorageError> {
        move |after, n| {
            assert!(n <= MAX_PAGE, "a store read never exceeds MAX_PAGE");
            let from = match after {
                None => 0,
                Some(p) => p.parse::<i64>().map_err(|_| StorageError::CursorExpired)? + 1,
            };
            Ok((from..len).take(usize::from(n)).collect())
        }
    }

    #[test]
    fn non_ascii_hex_is_expired_not_a_panic() {
        let binding = CursorBinding(String::new());
        for cursor in ["v1.aéa.x", "v1.éa.x", "v1.é.x", "v1.aaé.x"] {
            assert!(matches!(
                binding.decode(cursor),
                Err(StorageError::CursorExpired)
            ));
        }
    }

    #[test]
    fn has_more_only_when_a_row_follows_the_page() {
        let binding = CursorBinding("b".into());
        for (len, limit) in [(3, 3), (0, 3), (500, 500), (2, 3)] {
            let p = page(&binding, &request(limit, None), fetch(len), |r| {
                r.to_string()
            })
            .unwrap();
            assert_eq!(p.items.len() as i64, len);
            assert!(!p.has_more(), "{len} rows at limit {limit}");
        }
        for (len, limit) in [(4, 3), (501, 500)] {
            let p = page(&binding, &request(limit, None), fetch(len), |r| {
                r.to_string()
            })
            .unwrap();
            assert_eq!(p.items.len(), limit as usize);
            let next = page(&binding, &request(limit, p.next_cursor), fetch(len), |r| {
                r.to_string()
            })
            .unwrap();
            assert_eq!(next.items, vec![i64::from(limit as u16)]);
            assert!(!next.has_more());
        }
    }

    #[test]
    fn a_probe_error_is_returned() {
        let binding = CursorBinding("b".into());
        let mut reads = 0;
        let failing = |_: Option<&str>, n: u16| {
            reads += 1;
            if reads == 1 {
                Ok((0..i64::from(n)).collect())
            } else {
                Err(StorageError::Busy)
            }
        };
        assert!(matches!(
            page(&binding, &request(500, None), failing, |r: &i64| r
                .to_string()),
            Err(StorageError::Busy)
        ));
    }

    #[test]
    fn old_and_foreign_cursors_expire() {
        let binding = CursorBinding("b".into());
        let other = CursorBinding("c".into());
        let issued = other.cursor("1");
        for old in [
            "q1.0.1.2.id.abc",
            "l2.5.0.abc",
            "usage1.0.1.x",
            "aWI6MA",
            "3:abc",
            "1:2",
        ] {
            let r = page(
                &binding,
                &request(2, Some(BoundedText(old.into()))),
                fetch(5),
                |r| r.to_string(),
            );
            assert!(matches!(r, Err(StorageError::CursorExpired)), "{old}");
        }
        assert!(matches!(
            slice(&binding, vec![1, 2, 3], &request(1, Some(issued))),
            Err(StorageError::CursorExpired)
        ));
    }

    #[test]
    fn slice_pages_by_offset() {
        let binding = CursorBinding("b".into());
        let first = slice(&binding, vec![1, 2, 3], &request(2, None)).unwrap();
        assert_eq!(first.items.0, vec![1, 2]);
        assert!(first.has_more);
        let rest = slice(&binding, vec![1, 2, 3], &request(2, first.next_cursor)).unwrap();
        assert_eq!(rest.items.0, vec![3]);
        assert!(!rest.has_more && rest.next_cursor.is_none());
        let exact = slice(&binding, vec![1, 2], &request(2, None)).unwrap();
        assert!(!exact.has_more);
    }

    #[test]
    fn a_filtered_page_reads_on_until_it_fills() {
        let binding = CursorBinding("b".into());
        // Only even rows are visible, so a page of 300 spans two store batches.
        let even = |after: Option<&str>, n: u16| {
            let after = after
                .map(|p| p.parse::<i64>().map_err(|_| StorageError::CursorExpired))
                .transpose()?;
            scan(
                after,
                n,
                |after, batch| fetch(2000)(after.map(|a| a.to_string()).as_deref(), batch),
                |r| Some(*r),
                |r| Ok::<_, StorageError>(r % 2 == 0),
            )
        };
        let first = page(&binding, &request(300, None), even, |r| r.to_string()).unwrap();
        assert_eq!(first.items.len(), 300);
        assert_eq!(first.items.last(), Some(&598));
        assert!(first.has_more());
        let rest = page(&binding, &request(500, first.next_cursor), even, |r| {
            r.to_string()
        })
        .unwrap();
        assert_eq!(rest.items.len(), 500);
        assert_eq!(rest.items[0], 600);
        assert!(rest.has_more());
    }

    #[test]
    fn a_page_clipped_to_the_frame_continues_after_its_last_row() {
        let binding = CursorBinding("b".into());
        let p = page(&binding, &request(3, None), fetch(10), |r| r.to_string())
            .unwrap()
            .clip_to_frame(&binding, 4, |r| r.to_string())
            .unwrap();
        assert_eq!(p.items, vec![0, 1]);
        let next = page(&binding, &request(3, p.next_cursor), fetch(10), |r| {
            r.to_string()
        })
        .unwrap();
        assert_eq!(next.items, vec![2, 3, 4]);
    }
}
