-- A stable integer key associates a published passage with its contentless FTS row.
-- The original passage has an opaque text key, so its implicit SQLite rowid could change.
CREATE TABLE document_lexical_rows (
 id INTEGER PRIMARY KEY,
 block_id TEXT NOT NULL UNIQUE REFERENCES document_extracted_blocks(id)
) STRICT;

CREATE VIRTUAL TABLE document_lexical_fts USING fts5(
 text, content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2'
);
INSERT INTO document_lexical_fts(document_lexical_fts,rank) VALUES('secure-delete',1);

CREATE TRIGGER document_lexical_post AFTER INSERT ON document_lexical_rows BEGIN
 INSERT INTO document_lexical_fts(rowid,text)
 SELECT NEW.id,text FROM document_extracted_blocks WHERE id=NEW.block_id;
END;

CREATE TRIGGER document_lexical_unpost AFTER DELETE ON document_lexical_rows BEGIN
 DELETE FROM document_lexical_fts WHERE rowid=OLD.id;
END;

CREATE TRIGGER document_lexical_first_head AFTER INSERT ON document_index_versions
 WHEN NEW.published_revision IS NOT NULL
BEGIN
 INSERT INTO document_lexical_rows(block_id)
 SELECT b.id FROM document_extracted_blocks b JOIN document_extractions e ON e.id=b.extraction_id
 WHERE e.version_id=NEW.version_id AND e.revision=NEW.published_revision;
END;

CREATE TRIGGER document_lexical_replace_head AFTER UPDATE OF published_revision ON document_index_versions
 WHEN OLD.published_revision IS NOT NEW.published_revision
BEGIN
 DELETE FROM document_lexical_rows WHERE block_id IN (
   SELECT b.id FROM document_extracted_blocks b JOIN document_extractions e ON e.id=b.extraction_id
   WHERE e.version_id=OLD.version_id AND e.revision=OLD.published_revision
 );
 INSERT INTO document_lexical_rows(block_id)
 SELECT b.id FROM document_extracted_blocks b JOIN document_extractions e ON e.id=b.extraction_id
 WHERE e.version_id=NEW.version_id AND e.revision=NEW.published_revision;
END;

-- Ordinary version retirement uses a tombstone. Project purge deletes its prepared canonical
-- rows directly, so it must not delete other planned rows as a side effect.
CREATE TRIGGER document_lexical_retire_version BEFORE UPDATE OF deleted_at ON artifact_versions
 WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL
BEGIN
 DELETE FROM document_lexical_rows WHERE block_id IN (
   SELECT b.id FROM document_extracted_blocks b JOIN document_extractions e ON e.id=b.extraction_id
   WHERE e.version_id=OLD.id
 );
END;

-- Recreate the lexical projection for exact heads accepted before this migration.
INSERT INTO document_lexical_rows(block_id)
SELECT b.id FROM document_extracted_blocks b
 JOIN document_extractions e ON e.id=b.extraction_id
 JOIN document_index_versions i ON i.version_id=e.version_id AND i.published_revision=e.revision;
