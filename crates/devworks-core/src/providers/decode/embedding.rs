use super::*;

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
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(text)
                        .ok()
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
