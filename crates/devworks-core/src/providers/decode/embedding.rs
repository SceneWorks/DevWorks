use super::*;

fn base64(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(4) {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 4 * 3);
    let sextet = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let n = value.len() / 4;
    for (i, c) in value.as_bytes().as_chunks::<4>().0.iter().enumerate() {
        let a = sextet(c[0])?;
        let b = sextet(c[1])?;
        let padding = if c[2] == b'=' {
            2
        } else if c[3] == b'=' {
            1
        } else {
            0
        };
        if padding > 0 && i + 1 != n || padding == 2 && c[3] != b'=' {
            return None;
        }
        let d = if padding == 2 { 0 } else { sextet(c[2])? };
        let e = if padding > 0 { 0 } else { sextet(c[3])? };
        if padding == 2 && b & 15 != 0 || padding == 1 && d & 3 != 0 {
            return None;
        }
        bytes.push(a << 2 | b >> 4);
        if padding < 2 {
            bytes.push(b << 4 | d >> 2);
        }
        if padding == 0 {
            bytes.push(d << 6 | e);
        }
    }
    Some(bytes)
}
impl Decoder {
    pub(super) fn embedding(&mut self, v: &Value, raw: &str, out: &mut Vec<DecodedFrame>) {
        self.usage(raw, &["usage"], out);
        self.native(None, "embedding_response", v, raw, true, out);
        if let Some(error) = v.get("error") {
            self.provider_error(error, out);
            self.ended = true;
            return;
        }
        let Some(data) = v.get("data").and_then(Value::as_array) else {
            self.error(
                DecodeErrorKind::InvalidEmbedding,
                "missing_embedding_data",
                out,
            );
            self.ended = true;
            return;
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut dimensions = None;
        if data.is_empty() {
            self.error(
                DecodeErrorKind::InvalidEmbedding,
                "empty_embedding_data",
                out,
            );
        }
        for item in data {
            let Some(i) = index(item, "index") else {
                self.error(
                    DecodeErrorKind::InvalidEmbedding,
                    "missing_embedding_index",
                    out,
                );
                continue;
            };
            if !seen.insert(i) {
                self.error(
                    DecodeErrorKind::InvalidEmbedding,
                    "duplicate_embedding_index",
                    out,
                );
                continue;
            }
            let vector = match item.get("embedding") {
                Some(Value::Array(values)) if values.len() <= self.limits.embedding_dimensions => {
                    values
                        .iter()
                        .map(|v| v.as_f64().map(|v| v as f32).filter(|v| v.is_finite()))
                        .collect::<Option<Vec<_>>>()
                }
                Some(Value::String(text))
                    if text.len() <= self.limits.embedding_dimensions.saturating_mul(8) =>
                {
                    base64(text)
                        .filter(|b| b.len().is_multiple_of(4))
                        .map(|b| {
                            b.as_chunks::<4>()
                                .0
                                .iter()
                                .map(|c| f32::from_le_bytes(*c))
                                .collect::<Vec<_>>()
                        })
                        .filter(|v| v.iter().all(|x| x.is_finite()))
                }
                _ => None,
            };
            if let Some(vector) =
                vector.filter(|v| !v.is_empty() && v.len() <= self.limits.embedding_dimensions)
            {
                if dimensions.is_some_and(|n| n != vector.len())
                    || self.embedding_dimensions.is_some_and(|n| n != vector.len())
                {
                    self.error(
                        DecodeErrorKind::InvalidEmbedding,
                        "inconsistent_embedding_dimensions",
                        out,
                    );
                    continue;
                }
                dimensions = Some(vector.len());
                out.push(DecodedFrame::Embedding { index: i, vector });
            } else {
                self.error(
                    DecodeErrorKind::InvalidEmbedding,
                    "invalid_embedding_vector",
                    out,
                );
            }
        }
        self.stop = Some((DecodedOutcome::Completed, None));
        self.ended = true;
    }
}
