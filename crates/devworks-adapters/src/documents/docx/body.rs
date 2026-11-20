use super::{
    DocxError, DocxLimits, WordBody, WordCell, WordCoverageIssue, WordLink, WordLinkTarget,
    WordMerge, WordParagraph, WordStoryKind,
    fields::{self, MAX_CODE_BYTES},
    numbering::Numbering,
    opc::{self, Relationships, Target},
    styles::{ParagraphStyle, Styles, number},
    xml::{self, Element, REL, REL_STRICT, WORD, WORD_STRICT, Xml},
};
use devworks_core::{
    fingerprint::sha256_hex,
    protocol::{Id, data::Locator},
    services::documents::extraction::{
        ExtractedBlock, ExtractionControl, ExtractionError, ExtractionInput, Heading,
        normalize_text,
    },
};
use std::{collections::BTreeMap, sync::Arc};

const ENGINE: &str = "devworks-docx-v1";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Document,
    Body,
    Table,
    Row,
    Cell,
    Paragraph,
    Run,
    Text,
    FieldCode,
    ParagraphProperties,
    ListProperties,
    RowProperties,
    CellProperties,
    Hyperlink,
    Wrapper,
    Skip,
}

struct Paragraph {
    number: u32,
    cell: Option<Id>,
    properties: ParagraphStyle,
    text: String,
    hyperlinks: Vec<WordLink>,
}

#[derive(Default)]
struct TableRow {
    number: u32,
    column: u32,
    horizontal: Option<Id>,
    next_vertical: BTreeMap<(u32, u32), Id>,
}

struct Table {
    id: u32,
    row: TableRow,
    cell: Option<WordCell>,
    vertical: BTreeMap<(u32, u32), Id>,
}

struct FieldCapture {
    code: Option<String>,
    reading: bool,
}

struct Reader<'a, 'scope> {
    kind: WordStoryKind,
    source: &'a ExtractionInput<'scope>,
    relations: &'a Relationships,
    styles: &'a Styles,
    numbering: &'a Numbering,
    limits: &'a DocxLimits,
    control: &'a ExtractionControl<'scope>,
    body: WordBody,
    word: &'static str,
    frames: Vec<Frame>,
    seen_body: bool,
    inside_body: bool,
    paragraph: Option<Paragraph>,
    tables: Vec<Table>,
    part_id: Id,
    table_count: u32,
    paragraph_count: u32,
    blocks: usize,
    links: usize,
    fields: Vec<FieldCapture>,
    normalized_bytes: usize,
    headings: Arc<[Heading]>,
}

pub(super) fn read(
    bytes: &[u8],
    part: (&str, WordStoryKind),
    source: &ExtractionInput<'_>,
    relations: &Relationships,
    styles: (&Styles, &Numbering),
    limits: &DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<WordBody, DocxError> {
    let (part, kind) = part;
    let mut reader = Reader {
        kind,
        source,
        relations,
        styles: styles.0,
        numbering: styles.1,
        limits,
        control,
        body: WordBody {
            part: part.to_owned(),
            paragraphs: Vec::new(),
            cells: Vec::new(),
            coverage_issues: Default::default(),
            normalized_bytes: 0,
        },
        word: WORD,
        frames: Vec::new(),
        seen_body: false,
        inside_body: false,
        paragraph: None,
        tables: Vec::new(),
        part_id: Id::new(sha256_hex(part.as_bytes())).expect("SHA256 is an opaque ID"),
        table_count: 0,
        paragraph_count: 0,
        blocks: 0,
        links: 0,
        fields: Vec::new(),
        normalized_bytes: 0,
        headings: Arc::from([]),
    };
    xml::walk(bytes, limits.xml_depth, control, |depth, event| {
        reader.event(depth, event)
    })?;
    if !reader.seen_body {
        return Err(DocxError::InvalidBody);
    }
    if !reader.fields.is_empty() {
        reader
            .body
            .coverage_issues
            .insert(WordCoverageIssue::FieldCode);
    }
    reader
        .body
        .cells
        .sort_by_key(|cell| (cell.table, cell.row, cell.column));
    reader.body.normalized_bytes = reader.normalized_bytes;
    Ok(reader.body)
}

impl Reader<'_, '_> {
    fn event(&mut self, depth: usize, event: Xml<'_>) -> Result<(), DocxError> {
        match event {
            Xml::Start(element) => {
                let frame = self.start(depth, element)?;
                self.frames.push(frame);
            }
            Xml::Text(text) if self.frames.last() == Some(&Frame::Text) => {
                if self.fields.iter().all(|field| !field.reading) {
                    self.paragraph
                        .as_mut()
                        .ok_or(DocxError::InvalidBody)?
                        .text
                        .push_str(&text);
                }
            }
            Xml::Text(text) if self.frames.last() == Some(&Frame::FieldCode) => {
                if let Some(field) = self.fields.last_mut()
                    && field.reading
                    && let Some(code) = &mut field.code
                {
                    if code.len() + text.len() <= MAX_CODE_BYTES {
                        code.push_str(&text);
                    } else {
                        field.code = None;
                    }
                }
            }
            Xml::Text(text)
                if self.frames.last() != Some(&Frame::Skip) && !text.trim().is_empty() =>
            {
                return Err(DocxError::InvalidBody);
            }
            Xml::End => match self.frames.pop().ok_or(DocxError::InvalidBody)? {
                Frame::Paragraph => self.finish_paragraph()?,
                Frame::Cell => self.finish_cell()?,
                Frame::Row => {
                    let table = self.table()?;
                    table.vertical = std::mem::take(&mut table.row.next_vertical);
                }
                Frame::Table => {
                    self.tables.pop();
                }
                Frame::Body => self.inside_body = false,
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn start(&mut self, depth: usize, element: Element<'_>) -> Result<Frame, DocxError> {
        use Frame::*;
        let invalid = DocxError::InvalidBody;
        let name = element.name;
        if depth == 1 {
            self.word = match name.ns {
                WORD => WORD,
                WORD_STRICT => WORD_STRICT,
                _ => return Err(invalid),
            };
            if name.local != self.kind.root() {
                return Err(invalid);
            }
            if self.kind == WordStoryKind::Main {
                return Ok(Document);
            }
            self.inside_body = true;
            self.seen_body = true;
            return Ok(Body);
        }
        if depth == 2 && self.kind == WordStoryKind::Main && name.is(self.word, "body") {
            if std::mem::replace(&mut self.seen_body, true) {
                return Err(invalid);
            }
            self.inside_body = true;
            return Ok(Body);
        }
        if depth == 2
            && let Some(expected) = self.kind.entry_element()
        {
            if name.ns != self.word {
                return Ok(Skip);
            }
            if name.local != expected {
                return Err(invalid);
            }
            self.headings = Arc::from([]);
            if !self.fields.is_empty() {
                self.fields.clear();
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::FieldCode);
            }
            let separator = matches!(
                self.kind,
                WordStoryKind::Footnotes | WordStoryKind::Endnotes
            ) && element
                .attr(self.word, "type")
                .is_some_and(|kind| kind != "normal");
            return Ok(if separator { Skip } else { Wrapper });
        }
        if !self.inside_body {
            return Ok(Skip);
        }

        // Paragraph ordinals refer to the original XML part, including skipped Word paragraphs.
        if name.is(self.word, "p") {
            self.paragraph_count += 1;
            if self.paragraph_count as usize > self.limits.extraction.blocks {
                return Err(DocxError::StructureLimit);
            }
        }
        let parent = self
            .frames
            .iter()
            .rev()
            .copied()
            .find(|node| *node != Wrapper)
            .ok_or(invalid)?;
        if parent == Skip {
            return Ok(Skip);
        }
        if matches!(parent, Text | FieldCode) {
            return Err(invalid);
        }
        if name.ns != self.word {
            if name.ns.ends_with("/math") && matches!(name.local, "oMath" | "oMathPara") {
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::Equation);
            } else if name.is(
                "http://schemas.openxmlformats.org/markup-compatibility/2006",
                "AlternateContent",
            ) {
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::AlternateContent);
            }
            return Ok(Skip);
        }

        let frame = match (parent, name.local) {
            (Body | Cell, "p") => {
                if self.paragraph.is_some() {
                    return Err(invalid);
                }
                self.paragraph = Some(super::body::Paragraph {
                    number: self.paragraph_count,
                    cell: self.current_cell().map(|cell| cell.id.clone()),
                    properties: ParagraphStyle::default(),
                    text: String::new(),
                    hyperlinks: Vec::new(),
                });
                Paragraph
            }
            (Body | Cell, "tbl") => {
                self.table_count += 1;
                self.tables.push(super::body::Table {
                    id: self.table_count,
                    row: TableRow::initial(),
                    cell: None,
                    vertical: BTreeMap::new(),
                });
                Table
            }
            (Table, "tr") => {
                let table = self.table()?;
                table.row = TableRow::next(table.row.number);
                Row
            }
            (Row, "tc") => {
                self.open_cell()?;
                Cell
            }
            (Paragraph, "pPr") => ParagraphProperties,
            (ParagraphProperties, "numPr") => ListProperties,
            (ParagraphProperties, "pStyle" | "outlineLvl") | (ListProperties, "numId" | "ilvl") => {
                self.paragraph
                    .as_mut()
                    .ok_or(invalid)?
                    .properties
                    .apply(&element, self.word)?;
                Skip
            }
            (Row, "trPr") => RowProperties,
            (Cell, "tcPr") => CellProperties,
            (RowProperties, "gridBefore") => {
                let column = number(element.attr(self.word, "val").ok_or(invalid)?, 16_384)?;
                self.table()?.row.column = column;
                Skip
            }
            (CellProperties, "gridSpan" | "vMerge" | "hMerge") => {
                self.cell_property(&element)?;
                Skip
            }
            (Paragraph | Hyperlink, "hyperlink") => {
                self.link(&element)?;
                Hyperlink
            }
            (Paragraph | Hyperlink, "fldSimple") => {
                if let Some(code) = element.attr(self.word, "instr") {
                    self.field_link(Some(code))?;
                }
                Wrapper
            }
            (Run, "fldChar") => {
                self.field_marker(element.attr(self.word, "fldCharType"))?;
                Skip
            }
            (Run, "instrText") => FieldCode,
            (Paragraph | Hyperlink, "r") => Run,
            (Run, "t") => Text,
            (Run, "tab" | "ptab" | "br" | "cr" | "noBreakHyphen" | "softHyphen") => {
                let scalar = match name.local {
                    "tab" | "ptab" => '\t',
                    "br" | "cr" => '\n',
                    "noBreakHyphen" => '\u{2011}',
                    _ => '\u{00ad}',
                };
                if self.fields.iter().all(|field| !field.reading) {
                    self.paragraph.as_mut().ok_or(invalid)?.text.push(scalar);
                }
                Skip
            }
            (_, "sdt" | "sdtContent" | "smartTag" | "customXml" | "dir" | "bdo") => Wrapper,
            (_, "ins" | "moveTo") => {
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::RevisionMarkup);
                Wrapper
            }
            (
                _,
                "del" | "moveFrom" | "pPrChange" | "rPrChange" | "tblPrChange" | "tcPrChange"
                | "trPrChange",
            ) => {
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::RevisionMarkup);
                Skip
            }
            (_, "drawing" | "pict" | "txbxContent" | "object" | "altChunk" | "subDoc") => {
                self.body.coverage_issues.insert(match name.local {
                    "drawing" | "pict" => WordCoverageIssue::Drawing,
                    "txbxContent" => WordCoverageIssue::TextBox,
                    "object" => WordCoverageIssue::EmbeddedObject,
                    _ => WordCoverageIssue::ExternalContent,
                });
                Skip
            }
            (_, "p" | "tbl" | "tr" | "tc" | "t" | "body") => return Err(invalid),
            _ => Skip,
        };
        Ok(frame)
    }

    fn link(&mut self, element: &Element<'_>) -> Result<(), DocxError> {
        use DocxError::InvalidRelationships as Bad;
        let anchor = element
            .attr(self.word, "anchor")
            .filter(|value| !value.is_empty());
        if anchor.is_some_and(|value| value.len() > 512) {
            return Err(Bad);
        }
        let id = element
            .attr(REL, "id")
            .or_else(|| element.attr(REL_STRICT, "id"));
        let target = match (id, anchor) {
            (Some(id), _) => {
                let rel = self
                    .relations
                    .get(id)
                    .filter(|rel| opc::is_kind(&rel.kind, "hyperlink"))
                    .ok_or(Bad)?;
                match &rel.target {
                    Target::Internal(name) => WordLinkTarget::Part(name.clone()),
                    Target::External(uri) => WordLinkTarget::External(uri.clone()),
                }
            }
            (None, Some(anchor)) => WordLinkTarget::Bookmark(anchor.to_owned()),
            (None, None) => return Ok(()),
        };
        self.push_link(WordLink {
            target,
            anchor: anchor.map(str::to_owned),
        })
    }

    fn push_link(&mut self, link: WordLink) -> Result<(), DocxError> {
        let paragraph = self.paragraph.as_mut().ok_or(DocxError::InvalidBody)?;
        if self.links >= self.limits.extraction.blocks {
            return Err(DocxError::StructureLimit);
        }
        self.links += 1;
        paragraph.hyperlinks.push(link);
        Ok(())
    }

    fn field_marker(&mut self, marker: Option<&str>) -> Result<(), DocxError> {
        match marker {
            Some("begin") if self.fields.len() < self.limits.xml_depth => {
                self.fields.push(FieldCapture {
                    reading: true,
                    code: Some(String::new()),
                });
            }
            Some("begin") => return Err(DocxError::StructureLimit),
            Some("separate") if self.fields.last().is_some_and(|field| field.reading) => {
                let field = self.fields.last_mut().expect("guarded above");
                field.reading = false;
                let code = field.code.take();
                if self.fields.iter().all(|field| !field.reading) {
                    self.field_link(code.as_deref())?;
                }
            }
            Some("end") if !self.fields.is_empty() => {
                self.fields.pop();
            }
            _ => {
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::FieldCode);
            }
        }
        Ok(())
    }

    fn field_link(&mut self, code: Option<&str>) -> Result<(), DocxError> {
        let link = code
            .filter(|code| code.len() <= MAX_CODE_BYTES)
            .ok_or(())
            .and_then(fields::hyperlink);
        match link {
            Ok(Some(link)) => self.push_link(link)?,
            Ok(None) => {}
            Err(()) => {
                self.body
                    .coverage_issues
                    .insert(WordCoverageIssue::FieldCode);
            }
        }
        Ok(())
    }

    fn table(&mut self) -> Result<&mut Table, DocxError> {
        self.tables.last_mut().ok_or(DocxError::InvalidBody)
    }

    fn current_cell(&self) -> Option<&WordCell> {
        self.tables
            .iter()
            .rev()
            .find_map(|table| table.cell.as_ref())
    }

    fn open_cell(&mut self) -> Result<(), DocxError> {
        let enclosing_cell = self.current_cell().map(|cell| cell.id.clone());
        let part = self.part_id.clone();
        let table = self.table()?;
        let row = table.row.number;
        let column = table.row.column + 1;
        let id = Id::new(sha256_hex(
            format!("{}:{}:{row}:{column}", part.as_str(), table.id).as_bytes(),
        ))
        .expect("SHA256 is an opaque ID");
        table.cell = Some(WordCell {
            id,
            enclosing_cell,
            table: table.id,
            row,
            column,
            column_span: 1,
            vertical: None,
            horizontal: None,
            vertical_origin: None,
            horizontal_origin: None,
        });
        Ok(())
    }

    fn cell_property(&mut self, element: &Element<'_>) -> Result<(), DocxError> {
        let word = self.word;
        let cell = self.table()?.cell.as_mut().ok_or(DocxError::InvalidBody)?;
        let value = element.attr(word, "val");
        match element.name.local {
            "gridSpan" => {
                let span = number(value.ok_or(DocxError::InvalidBody)?, 16_384)?;
                if span == 0 {
                    return Err(DocxError::InvalidBody);
                }
                cell.column_span = span;
            }
            "vMerge" => cell.vertical = Some(merge(value)?),
            "hMerge" => cell.horizontal = Some(merge(value)?),
            _ => unreachable!("cell property selected by caller"),
        }
        Ok(())
    }

    fn finish_cell(&mut self) -> Result<(), DocxError> {
        let invalid = DocxError::InvalidBody;
        if self.body.cells.len() >= self.limits.extraction.blocks {
            return Err(DocxError::StructureLimit);
        }
        let table = self.table()?;
        let mut cell = table.cell.take().ok_or(invalid)?;
        let grid = (cell.column, cell.column_span);
        if let Some(kind) = cell.vertical {
            let origin = match kind {
                WordMerge::Start => cell.id.clone(),
                WordMerge::Continue => table.vertical.get(&grid).cloned().ok_or(invalid)?,
            };
            table.row.next_vertical.insert(grid, origin.clone());
            cell.vertical_origin = Some(origin);
        }
        cell.horizontal_origin = match cell.horizontal {
            Some(WordMerge::Start) => Some(cell.id.clone()),
            Some(WordMerge::Continue) => Some(table.row.horizontal.clone().ok_or(invalid)?),
            None => None,
        };
        table.row.horizontal = cell.horizontal_origin.clone();
        table.row.column = table
            .row
            .column
            .checked_add(cell.column_span)
            .filter(|column| *column <= 16_384)
            .ok_or(invalid)?;
        self.body.cells.push(cell);
        Ok(())
    }

    fn finish_paragraph(&mut self) -> Result<(), DocxError> {
        let paragraph = self.paragraph.take().ok_or(DocxError::InvalidBody)?;
        let ceiling = &self.limits.extraction;
        let remaining = ceiling.normalized_bytes - self.normalized_bytes;
        let text = normalize_text(&paragraph.text, remaining, self.control)?;
        self.normalized_bytes += text.len();
        let (heading_level, list) = self.styles.effective(
            &paragraph.properties,
            self.numbering,
            self.limits.xml_depth,
            self.control,
        )?;
        if let Some(level) = heading_level {
            let mut path: Vec<Heading> = self
                .headings
                .iter()
                .take_while(|heading| heading.level < level)
                .cloned()
                .collect();
            let title = text.trim();
            path.push(Heading {
                level,
                title: title[..title.floor_char_boundary(256)].to_owned(),
            });
            self.headings = Arc::from(path);
        }
        let mut blocks = Vec::new();
        let mut tail = text.as_str();
        while !tail.is_empty() {
            self.control.check()?;
            let end = tail.floor_char_boundary(ceiling.chunk_bytes);
            let fragment = &tail[..end];
            tail = &tail[end..];
            if fragment.trim().is_empty() {
                continue;
            }
            if self.blocks >= ceiling.blocks {
                return Err(ExtractionError::BlockLimit.into());
            }
            blocks.push(ExtractedBlock::new(
                self.source,
                ENGINE,
                Locator::Docx {
                    part: self.part_id.clone(),
                    paragraph: paragraph.number,
                    table_cell: paragraph.cell.clone(),
                },
                self.blocks,
                self.headings.clone(),
                fragment.to_owned(),
            ));
            self.blocks += 1;
        }
        self.body.paragraphs.push(WordParagraph {
            number: paragraph.number,
            style: paragraph.properties.name,
            heading_level,
            list,
            cell: paragraph.cell,
            hyperlinks: paragraph.hyperlinks,
            blocks,
        });
        Ok(())
    }
}

impl TableRow {
    fn initial() -> Self {
        Self {
            number: 0,
            ..Self::default()
        }
    }

    fn next(previous: u32) -> Self {
        Self {
            number: previous + 1,
            ..Self::default()
        }
    }
}

fn merge(value: Option<&str>) -> Result<WordMerge, DocxError> {
    match value {
        Some("restart") => Ok(WordMerge::Start),
        None | Some("continue") => Ok(WordMerge::Continue),
        _ => Err(DocxError::InvalidBody),
    }
}
