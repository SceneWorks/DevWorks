-- Scope the document activity projection to preparation rows for one retained source version.
CREATE INDEX document_embedding_preparation_source ON operations(
 json_extract(payload_json,'$.origin.source.versionId'))
 WHERE capability='conventions.embedding_prepare'
   AND json_extract(payload_json,'$.origin.kind')='DocumentIndex';
