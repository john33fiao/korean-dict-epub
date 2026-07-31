CREATE TABLE kweb_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format_marker TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
    schema_fingerprint TEXT NOT NULL CHECK (length(schema_fingerprint) = 64),
    canonical_id_schema TEXT NOT NULL,
    record_schema TEXT NOT NULL,
    derived_table_prefix TEXT NOT NULL CHECK (derived_table_prefix = 'derived_')
) STRICT, WITHOUT ROWID;

INSERT INTO kweb_metadata (
    singleton,
    format_marker,
    schema_version,
    schema_fingerprint,
    canonical_id_schema,
    record_schema,
    derived_table_prefix
) VALUES (
    1,
    'korean-dict-web-db',
    1,
    '0000000000000000000000000000000000000000000000000000000000000000',
    'kweb-canonical-id-v1',
    'kdep-source-record-v1',
    'derived_'
);

CREATE TABLE schema_migration (
    version INTEGER PRIMARY KEY CHECK (version >= 1),
    name TEXT NOT NULL UNIQUE CHECK (name <> ''),
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64)
) STRICT, WITHOUT ROWID;

CREATE TABLE corpus (
    corpus_id TEXT PRIMARY KEY CHECK (corpus_id <> ''),
    source_commit TEXT NOT NULL UNIQUE
        CHECK (length(source_commit) = 40 AND source_commit NOT GLOB '*[^0-9a-f]*'),
    state TEXT NOT NULL CHECK (state IN ('importing', 'ready', 'failed')),
    source_file_count INTEGER CHECK (source_file_count >= 0),
    entry_count INTEGER CHECK (entry_count >= 0),
    CHECK (
        state <> 'ready'
        OR (source_file_count IS NOT NULL AND entry_count IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE source_file (
    corpus_id TEXT NOT NULL,
    relative_path TEXT NOT NULL CHECK (relative_path <> ''),
    dictionary TEXT NOT NULL CHECK (dictionary IN ('krdict', 'stdict', 'opendict')),
    source_ordinal INTEGER NOT NULL CHECK (source_ordinal >= 0),
    volume_number INTEGER NOT NULL CHECK (volume_number >= 1),
    volume_total INTEGER NOT NULL CHECK (volume_total >= volume_number),
    record_sha256 TEXT CHECK (
        record_sha256 IS NULL
        OR (length(record_sha256) = 64 AND record_sha256 NOT GLOB '*[^0-9a-f]*')
    ),
    record_count INTEGER CHECK (record_count >= 0),
    entry_count INTEGER CHECK (entry_count >= 0),
    PRIMARY KEY (corpus_id, relative_path),
    UNIQUE (corpus_id, relative_path, dictionary),
    UNIQUE (corpus_id, source_ordinal),
    UNIQUE (corpus_id, dictionary, volume_number),
    FOREIGN KEY (corpus_id) REFERENCES corpus(corpus_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE entity (
    canonical_id TEXT PRIMARY KEY CHECK (canonical_id LIKE 'kweb:v1/%'),
    corpus_id TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    dictionary TEXT NOT NULL CHECK (dictionary IN ('krdict', 'stdict', 'opendict')),
    entity_kind TEXT NOT NULL
        CHECK (entity_kind IN ('entry', 'part_of_speech', 'common_pattern', 'sense')),
    native_key TEXT,
    parent_entry_id TEXT,
    source_locator TEXT NOT NULL CHECK (source_locator <> ''),
    entry_ordinal INTEGER NOT NULL CHECK (entry_ordinal >= 0),
    kind_ordinal INTEGER NOT NULL CHECK (kind_ordinal >= 0),
    UNIQUE (corpus_id, source_locator),
    UNIQUE (canonical_id, corpus_id, relative_path),
    FOREIGN KEY (corpus_id, relative_path, dictionary)
        REFERENCES source_file(corpus_id, relative_path, dictionary) ON DELETE CASCADE,
    FOREIGN KEY (parent_entry_id) REFERENCES entity(canonical_id) ON DELETE CASCADE,
    CHECK (
        (entity_kind = 'entry' AND parent_entry_id IS NULL)
        OR (entity_kind <> 'entry' AND parent_entry_id IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE source_record (
    corpus_id TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    record_ordinal INTEGER NOT NULL CHECK (record_ordinal >= 0),
    entry_id TEXT,
    record_kind TEXT NOT NULL CHECK (
        record_kind IN (
            'start_element',
            'empty_element',
            'element_text',
            'tail_text',
            'end_element'
        )
    ),
    depth INTEGER NOT NULL CHECK (depth >= 0),
    qname TEXT,
    text_value TEXT,
    PRIMARY KEY (corpus_id, relative_path, record_ordinal),
    FOREIGN KEY (corpus_id, relative_path)
        REFERENCES source_file(corpus_id, relative_path) ON DELETE CASCADE,
    FOREIGN KEY (entry_id, corpus_id, relative_path)
        REFERENCES entity(canonical_id, corpus_id, relative_path) ON DELETE CASCADE,
    CHECK (
        (
            record_kind IN ('start_element', 'empty_element', 'end_element')
            AND qname IS NOT NULL
            AND qname <> ''
            AND text_value IS NULL
        )
        OR (
            record_kind IN ('element_text', 'tail_text')
            AND qname IS NULL
            AND text_value IS NOT NULL
        )
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE source_attribute (
    corpus_id TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    record_ordinal INTEGER NOT NULL,
    attribute_ordinal INTEGER NOT NULL CHECK (attribute_ordinal >= 0),
    qname TEXT NOT NULL CHECK (qname <> ''),
    attribute_value TEXT NOT NULL,
    PRIMARY KEY (corpus_id, relative_path, record_ordinal, attribute_ordinal),
    FOREIGN KEY (corpus_id, relative_path, record_ordinal)
        REFERENCES source_record(corpus_id, relative_path, record_ordinal) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE entry_projection (
    entry_id TEXT PRIMARY KEY,
    headword TEXT,
    homonym_number TEXT,
    headword_record_ordinal INTEGER CHECK (headword_record_ordinal >= 0),
    FOREIGN KEY (entry_id) REFERENCES entity(canonical_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE text_projection (
    entry_id TEXT NOT NULL,
    projection_ordinal INTEGER NOT NULL CHECK (projection_ordinal >= 0),
    entity_id TEXT NOT NULL,
    field_kind TEXT NOT NULL CHECK (field_kind <> ''),
    text_value TEXT NOT NULL,
    source_record_ordinal INTEGER NOT NULL CHECK (source_record_ordinal >= 0),
    PRIMARY KEY (entry_id, projection_ordinal),
    FOREIGN KEY (entry_id) REFERENCES entity(canonical_id) ON DELETE CASCADE,
    FOREIGN KEY (entity_id) REFERENCES entity(canonical_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE relation (
    relation_id TEXT PRIMARY KEY CHECK (relation_id <> ''),
    source_entity_id TEXT NOT NULL,
    source_entry_id TEXT NOT NULL,
    relation_ordinal INTEGER NOT NULL CHECK (relation_ordinal >= 0),
    target_namespace TEXT,
    status TEXT NOT NULL
        CHECK (status IN ('resolved', 'self_reference', 'unresolved', 'ambiguous')),
    resolved_target_id TEXT,
    resolved_target_entry_id TEXT,
    reason TEXT NOT NULL CHECK (reason <> ''),
    in_cycle INTEGER NOT NULL DEFAULT 0 CHECK (in_cycle IN (0, 1)),
    UNIQUE (source_entity_id, relation_ordinal),
    FOREIGN KEY (source_entity_id) REFERENCES entity(canonical_id) ON DELETE CASCADE,
    FOREIGN KEY (source_entry_id) REFERENCES entity(canonical_id) ON DELETE CASCADE,
    FOREIGN KEY (resolved_target_id) REFERENCES entity(canonical_id),
    FOREIGN KEY (resolved_target_entry_id) REFERENCES entity(canonical_id),
    CHECK (
        (
            status IN ('resolved', 'self_reference')
            AND resolved_target_id IS NOT NULL
            AND resolved_target_entry_id IS NOT NULL
        )
        OR (
            status IN ('unresolved', 'ambiguous')
            AND resolved_target_id IS NULL
            AND resolved_target_entry_id IS NULL
        )
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE relation_raw_field (
    relation_id TEXT NOT NULL,
    field_kind TEXT NOT NULL
        CHECK (field_kind IN ('type', 'target_key', 'word', 'homonym', 'unit', 'url')),
    field_ordinal INTEGER NOT NULL CHECK (field_ordinal >= 0),
    field_value TEXT NOT NULL,
    PRIMARY KEY (relation_id, field_kind, field_ordinal),
    FOREIGN KEY (relation_id) REFERENCES relation(relation_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE relation_candidate (
    relation_id TEXT NOT NULL,
    candidate_ordinal INTEGER NOT NULL CHECK (candidate_ordinal >= 0),
    candidate_target_id TEXT NOT NULL,
    PRIMARY KEY (relation_id, candidate_target_id),
    UNIQUE (relation_id, candidate_ordinal),
    FOREIGN KEY (relation_id) REFERENCES relation(relation_id) ON DELETE CASCADE,
    FOREIGN KEY (candidate_target_id) REFERENCES entity(canonical_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX entity_namespace_native_key_idx
    ON entity(corpus_id, dictionary, entity_kind, native_key);
CREATE INDEX entity_source_order_idx
    ON entity(corpus_id, relative_path, entry_ordinal, kind_ordinal, canonical_id);
CREATE INDEX entity_parent_idx
    ON entity(parent_entry_id, entity_kind, kind_ordinal, canonical_id);
CREATE INDEX source_record_entry_order_idx
    ON source_record(entry_id, record_ordinal);
CREATE INDEX text_projection_source_order_idx
    ON text_projection(entry_id, source_record_ordinal, projection_ordinal);
CREATE INDEX relation_source_order_idx
    ON relation(source_entry_id, relation_ordinal, relation_id);
CREATE INDEX relation_resolved_target_idx
    ON relation(resolved_target_entry_id, status, relation_id);
