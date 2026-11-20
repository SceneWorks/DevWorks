//! Direct page text in content-stream order. Unavailable operands, forms and ambiguous
//! positioning are reported as partial instead of claiming complete visual reading order.
use super::{STREAM_LIMIT, check, parse_error};
use devworks_core::services::documents::{extraction::ExtractionControl, pdf::PdfFailure};
use lopdf::{Dictionary, Document, Encoding, Object, ObjectId, content::Content};
use std::collections::BTreeMap;

pub(super) fn page(
    doc: &Document,
    id: ObjectId,
    maximum: usize,
    control: &ExtractionControl<'_>,
) -> Result<(String, bool), PdfFailure> {
    let content = doc
        .get_page_content_with_limit(id, STREAM_LIMIT)
        .and_then(|bytes| Content::decode_strict(&bytes))
        .map_err(|error| parse_error(&error))?;
    let fonts = doc
        .get_page_fonts(id)
        .map_err(|error| parse_error(&error))?;
    let (direct, inherited) = doc
        .get_page_resources(id)
        .map_err(|error| parse_error(&error))?;
    let resources = direct.or_else(|| {
        inherited
            .first()
            .and_then(|id| doc.get_dictionary(*id).ok())
    });
    let pictures = resources.and_then(|resources| {
        resources
            .get_deref(b"XObject", doc)
            .and_then(Object::as_dict)
            .ok()
    });

    let mut collector = Text {
        value: String::new(),
        limit: maximum,
        partial: false,
    };
    let mut encodings = BTreeMap::new();
    let mut current = None;
    let mut saved = Vec::new();
    let mut baseline: Option<f32> = None;

    for operation in &content.operations {
        check(control)?;
        let operands = operation.operands.as_slice();
        match operation.operator.as_str() {
            "BT" | "ET" | "T*" => {
                collector.separator('\n')?;
                baseline = None;
            }
            "q" if saved.len() < 128 => saved.push(current),
            "Q" => current = saved.pop().unwrap_or_default(),
            "q" => return Err(PdfFailure::StreamLimit),
            "Tf" => {
                current = operands.first().and_then(|object| object.as_name().ok());
                if let Some(name) = current
                    && let Some(font) = fonts.get(name)
                    && !encodings.contains_key(name)
                {
                    let encoding = match font.get_font_encoding_with_limit(doc, STREAM_LIMIT) {
                        Ok(value) => Some(value),
                        Err(error) if parse_error(&error) == PdfFailure::StreamLimit => {
                            return Err(PdfFailure::StreamLimit);
                        }
                        Err(_) => None,
                    };
                    encodings.insert(name, encoding);
                }
            }
            "Td" | "TD" => {
                if let [x, y] = operands
                    && let (Ok(x), Ok(y)) = (x.as_float(), y.as_float())
                {
                    baseline = Some(baseline.unwrap_or_default() + y);
                    collector.position(x, y)?;
                } else {
                    collector.partial = true;
                }
            }
            "Tm" => {
                if let [_, b, c, _, x, y] = operands
                    && let (Ok(x), Ok(y), Ok(b), Ok(c)) =
                        (x.as_float(), y.as_float(), b.as_float(), c.as_float())
                {
                    collector.partial |= b != 0.0 || c != 0.0;
                    if let Some(previous) = baseline {
                        collector.position(x, y - previous)?;
                    }
                    baseline = Some(y);
                } else {
                    collector.partial = true;
                }
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if matches!(operation.operator.as_str(), "'" | "\"") {
                    collector.separator('\n')?;
                    baseline = None;
                }
                let encoding = current
                    .and_then(|name| encodings.get(name))
                    .and_then(Option::as_ref);
                if let Some(encoding) = encoding {
                    if let Some(value) = operands.last() {
                        collector.show(value, encoding, control)?;
                    } else {
                        collector.partial = true;
                    }
                } else {
                    collector.partial = true;
                }
            }
            "Do" if !is_image(operands, pictures, doc) => collector.partial = true,
            "BDC" | "DP"
                if operands.first().and_then(|tag| tag.as_name().ok()) == Some(b"OC")
                    || operands
                        .get(1)
                        .and_then(|props| props.as_dict().ok())
                        .is_some_and(|props| props.has(b"ActualText")) =>
            {
                collector.partial = true;
            }
            _ => {}
        }
    }
    Ok((collector.value, collector.partial))
}

fn is_image(operands: &[Object], pictures: Option<&Dictionary>, doc: &Document) -> bool {
    let kind = operands
        .first()
        .and_then(|operand| operand.as_name().ok())
        .and_then(|name| pictures?.get_deref(name, doc).ok())
        .and_then(|object| object.as_stream().ok())
        .and_then(|stream| stream.dict.get(b"Subtype").ok())
        .and_then(|subtype| subtype.as_name().ok());
    kind == Some(b"Image")
}

struct Text {
    value: String,
    limit: usize,
    partial: bool,
}

impl Text {
    fn position(&mut self, x: f32, y: f32) -> Result<(), PdfFailure> {
        if y != 0.0 {
            self.separator('\n')?;
        } else if x != 0.0 && !self.value.is_empty() && !self.value.ends_with('\n') {
            self.partial = true;
            self.separator(' ')?;
        }
        Ok(())
    }

    fn separator(&mut self, scalar: char) -> Result<(), PdfFailure> {
        if !self.value.is_empty() && !self.value.ends_with(char::is_whitespace) {
            self.push(scalar.encode_utf8(&mut [0; 4]))?;
        }
        Ok(())
    }

    fn show(
        &mut self,
        object: &Object,
        encoding: &Encoding<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<(), PdfFailure> {
        if let Object::Array(items) = object {
            for item in items {
                check(control)?;
                match item {
                    Object::Integer(_) | Object::Real(_)
                        if item.as_float().is_ok_and(|advance| advance < -120.0) =>
                    {
                        self.separator(' ')?
                    }
                    Object::Integer(_) | Object::Real(_) => {}
                    value => self.decode(value, encoding)?,
                }
            }
        } else {
            self.decode(object, encoding)?;
        }
        Ok(())
    }

    fn decode(&mut self, object: &Object, encoding: &Encoding<'_>) -> Result<(), PdfFailure> {
        match object
            .as_str()
            .and_then(|bytes| Document::decode_text(encoding, bytes))
        {
            Ok(text) => {
                self.partial |= text.contains('\u{fffd}');
                self.push(&text)
            }
            Err(_) => {
                self.partial = true;
                Ok(())
            }
        }
    }

    fn push(&mut self, text: &str) -> Result<(), PdfFailure> {
        if text.len() > self.limit - self.value.len() {
            return Err(PdfFailure::OutputLimit);
        }
        self.value.push_str(text);
        Ok(())
    }
}
