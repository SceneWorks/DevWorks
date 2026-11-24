-- Scope the document activity projection to preparation rows for one retained source version.
CREATE INDEX document_embedding_preparation_source ON operations(
 json_extract(payload_json,'$.origin.source.versionId'))
 WHERE capability='conventions.embedding_prepare'
   AND json_extract(payload_json,'$.origin.kind')='DocumentIndex';

-- Publication belongs to the exact retained index even when the original owner's events purge.
ALTER TABLE document_index_versions ADD COLUMN published_at INTEGER
 CHECK(published_at IS NULL OR (published_at>=0 AND published_generation IS NOT NULL));

UPDATE document_index_versions SET published_at = (
 SELECT created_at FROM events WHERE aggregate_kind='document_index_run'
  AND aggregate_id=published_generation
  AND kind IN('DocumentIndexPublished','DocumentIndexPartiallyPublished',
    'DocumentIndexVisionHeld','DocumentIndexEmbeddingHeld')
 ORDER BY seq DESC LIMIT 1
) WHERE published_generation IS NOT NULL;
