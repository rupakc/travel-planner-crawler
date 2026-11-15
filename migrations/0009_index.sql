-- 0009 search index: FTS columns and embeddings (owner: vc-index 15xx). Spec §5.11, §17.9.
SET lock_timeout = '5s';

ALTER TABLE records
    ADD COLUMN fts_config regconfig NOT NULL DEFAULT 'simple',
    ADD COLUMN search_text text,
    ADD COLUMN tsv tsvector;
CREATE INDEX records_tsv ON records USING gin (tsv);

-- One table for every model; a partial HNSW index per model with a fixed dimension.
CREATE TABLE embeddings (
    owner_type   text NOT NULL CHECK (owner_type IN ('record', 'entity', 'article')),
    owner_id     text NOT NULL,
    model_id     text NOT NULL,
    dim          integer NOT NULL CHECK (dim > 0),
    vec          halfvec NOT NULL,
    content_hash bytea NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_type, owner_id, model_id),
    CHECK (vector_dims(vec) = dim)
);
CREATE INDEX embeddings_hnsw_text_embedding_005 ON embeddings
    USING hnsw ((vec::halfvec(768)) halfvec_cosine_ops) WITH (m = 16, ef_construction = 64)
    WHERE model_id = 'text-embedding-005';
