-- One rebuildable unit vector per claim and qualified embedding space, of the claim's current
-- text (text_digest: SHA-256 of the text that was embedded). A text change or deletion of the
-- claim removes its vectors in the same write; scope is read from the claim at query time. A
-- row without a vector records that the route returned no usable vector for that text, so the
-- text is not embedded again until it changes.
CREATE TABLE knowledge_claim_vectors (
 claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
 space_id TEXT NOT NULL,
 dimensions INTEGER NOT NULL,
 text_digest TEXT NOT NULL CHECK(length(text_digest)=64),
 vector BLOB CHECK(vector IS NULL OR length(vector)=dimensions*4),
 PRIMARY KEY(claim_id,space_id),
 FOREIGN KEY(space_id,dimensions) REFERENCES embedding_spaces(id,dimensions)
) STRICT;
CREATE INDEX knowledge_claim_vectors_space ON knowledge_claim_vectors(space_id,claim_id);

-- The captured embedding inputs of a claim (its text), found when the claim is deleted or its
-- text changes so that their versions are retired in the same write.
CREATE INDEX artifacts_knowledge_embedding_input
 ON artifacts(json_extract(provenance_json,'$.derived.knowledgeClaimId'))
 WHERE json_extract(provenance_json,'$.derived.kind')='KnowledgeEmbeddingInput';
