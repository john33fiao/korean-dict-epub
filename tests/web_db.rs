use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::record::{CanonicalDigest, SourceAttribute, SourceRecord};
use korean_dict_epub::web_db::{
    APPLICATION_ID, FORMAT_MARKER, LATEST_SCHEMA_VERSION, ValidationLevel, WebDbError, create_new,
    open_and_migrate, validate,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const COMMIT: &str = "42c0d01889f34536e9cf94fe57f62bd2055b1bde";

struct TempDatabase {
    root: PathBuf,
    path: PathBuf,
}

impl TempDatabase {
    fn new(label: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "korean-dict-web-db-{label}-{}-{sequence}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("stale fixture should be removable");
        }
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let path = root.join("dictionary.sqlite");
        Self { root, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn creates_valid_strict_schema_without_clobbering_or_readonly_side_effects() {
    let fixture = TempDatabase::new("fresh");
    let connection = create_new(fixture.path()).expect("fresh migration should pass");
    assert_eq!(
        pragma_i64(&connection, "application_id"),
        i64::from(APPLICATION_ID)
    );
    assert_eq!(
        pragma_i64(&connection, "user_version"),
        LATEST_SCHEMA_VERSION
    );
    assert_all_application_tables_are_strict_without_rowid(&connection);
    drop(connection);

    let before = fs::read(fixture.path()).unwrap();
    let descriptor = validate(fixture.path(), ValidationLevel::SchemaOnly).unwrap();
    assert_eq!(descriptor.format_marker, FORMAT_MARKER);
    assert_eq!(descriptor.schema_version, 1);
    assert!(descriptor.corpus.is_none());
    assert_eq!(before, fs::read(fixture.path()).unwrap());
    for suffix in ["-journal", "-wal", "-shm"] {
        assert!(!PathBuf::from(format!("{}{suffix}", fixture.path().display())).exists());
    }

    let reopened = open_and_migrate(fixture.path()).expect("latest migration should be a no-op");
    assert_eq!(pragma_i64(&reopened, "user_version"), 1);
    drop(reopened);
    let error = create_new(fixture.path()).expect_err("create must never overwrite");
    assert!(matches!(error, WebDbError::ExistingPath(_)));

    let sidecar_fixture = TempDatabase::new("sidecar");
    let sidecar = PathBuf::from(format!("{}-wal", sidecar_fixture.path().display()));
    fs::write(&sidecar, b"not-owned-by-create-new").unwrap();
    let error = create_new(sidecar_fixture.path()).expect_err("sidecar must not be overwritten");
    assert!(matches!(error, WebDbError::ExistingPath(_)));
    assert_eq!(fs::read(&sidecar).unwrap(), b"not-owned-by-create-new");
}

#[test]
fn rejects_wrong_identity_future_versions_checksum_drift_and_symlinks() {
    let wrong = TempDatabase::new("wrong-app");
    let connection = Connection::open(wrong.path()).unwrap();
    connection
        .pragma_update(None, "application_id", 0x1234_i64)
        .unwrap();
    drop(connection);
    let error = validate(wrong.path(), ValidationLevel::SchemaOnly).unwrap_err();
    assert!(matches!(error, WebDbError::WrongApplicationId { .. }));

    let future = TempDatabase::new("future");
    let connection = create_new(future.path()).unwrap();
    connection
        .pragma_update(None, "user_version", 2_i64)
        .unwrap();
    drop(connection);
    let error = validate(future.path(), ValidationLevel::SchemaOnly).unwrap_err();
    assert!(matches!(
        error,
        WebDbError::UnsupportedSchemaVersion { found: 2, .. }
    ));

    let drift = TempDatabase::new("checksum");
    let connection = create_new(drift.path()).unwrap();
    connection
        .execute(
            "UPDATE schema_migration SET sha256 = ?1 WHERE version = 1",
            params!["f".repeat(64)],
        )
        .unwrap();
    drop(connection);
    let error = open_and_migrate(drift.path()).unwrap_err();
    assert!(matches!(error, WebDbError::MigrationMismatch { .. }));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = drift.root.join("linked.sqlite");
        symlink(drift.path(), &link).unwrap();
        let error = validate(&link, ValidationLevel::SchemaOnly).unwrap_err();
        assert!(matches!(error, WebDbError::InvalidPath { .. }));
    }
}

#[test]
fn stores_namespaced_entities_all_relation_states_and_ready_metadata() {
    let fixture = TempDatabase::new("ready");
    let mut connection = create_new(fixture.path()).unwrap();
    populate_ready_fixture(&mut connection);
    drop(connection);

    let descriptor = validate(fixture.path(), ValidationLevel::ReadyCorpus).unwrap();
    let corpus = descriptor.corpus.unwrap();
    assert_eq!(corpus.source_commit, COMMIT);
    assert_eq!(corpus.source_file_count, 3);
    assert_eq!(corpus.entry_count, 4);
    assert_eq!(corpus.representative_headword, "가ᄀᆞ");

    let connection = Connection::open(fixture.path()).unwrap();
    let (native_count, canonical_count): (i64, i64) = connection
        .query_row(
            "SELECT count(DISTINCT native_key), count(DISTINCT canonical_id) \
             FROM entity WHERE entity_kind = 'entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        native_count, 1,
        "all fixture dictionaries intentionally share key 001"
    );
    assert_eq!(
        canonical_count, 4,
        "canonical IDs must not merge colliding keys"
    );
    let stored_native_key: String = connection
        .query_row(
            "SELECT native_key FROM entity \
             WHERE dictionary = 'stdict' AND entity_kind = 'entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_native_key, "001",
        "native keys retain leading zeroes"
    );

    let statuses = connection
        .prepare("SELECT status FROM relation ORDER BY relation_ordinal")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        statuses,
        ["resolved", "self_reference", "unresolved", "ambiguous"]
    );
    let standard_kinds: i64 = connection
        .query_row(
            "SELECT count(DISTINCT entity_kind) FROM entity WHERE dictionary = 'stdict'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(standard_kinds, 4);

    let entity_query_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN \
             SELECT canonical_id FROM entity \
             WHERE corpus_id = ?1 AND dictionary = 'stdict' \
               AND entity_kind = 'entry' AND native_key = '001'",
            params![COMMIT],
            |row| row.get(3),
        )
        .unwrap();
    assert!(entity_query_plan.contains("entity_namespace_native_key_idx"));

    let relation_query_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN \
             SELECT relation_id FROM relation \
             WHERE resolved_target_entry_id = ?1 AND status = 'resolved'",
            params![entity_id("stdict", "entry/001")],
            |row| row.get(3),
        )
        .unwrap();
    assert!(relation_query_plan.contains("relation_resolved_target_idx"));
}

#[test]
fn ready_validation_rejects_entities_outside_the_corpus_namespace() {
    let fixture = TempDatabase::new("wrong-namespace");
    let mut connection = create_new(fixture.path()).unwrap();
    populate_ready_fixture(&mut connection);
    connection
        .execute(
            "INSERT INTO entity( \
                 canonical_id, corpus_id, relative_path, dictionary, entity_kind, native_key, \
                 parent_entry_id, source_locator, entry_ordinal, kind_ordinal \
             ) VALUES (?1, ?2, 'krdict/001.xml', 'krdict', 'sense', '999', ?3, \
                       'krdict:krdict/001.xml#entry=1/sense=999', 0, 999)",
            params![
                format!("kweb:v1/{}/krdict/entry/001/sense/999", "f".repeat(40)),
                COMMIT,
                entity_id("krdict", "entry/001")
            ],
        )
        .unwrap();
    drop(connection);

    let error = validate(fixture.path(), ValidationLevel::ReadyCorpus).unwrap_err();
    assert!(
        matches!(error, WebDbError::InvalidDatabase { reason, .. } if reason.contains("outside its namespace"))
    );
}

#[test]
fn reconstructs_lossless_records_and_detects_field_or_order_changes() {
    let fixture = TempDatabase::new("lossless");
    let mut connection = create_new(fixture.path()).unwrap();
    populate_ready_fixture(&mut connection);

    let records = lossless_records();
    insert_records(&mut connection, &records);
    let reconstructed = read_records(&connection);
    assert_eq!(reconstructed, records);
    assert_eq!(digest(&reconstructed), digest(&records));

    let mut missing_field = reconstructed.clone();
    missing_field.remove(2);
    assert_ne!(digest(&missing_field), digest(&records));

    let mut reordered_attributes = reconstructed;
    if let SourceRecord::StartElement { attributes, .. } = &mut reordered_attributes[0] {
        attributes.reverse();
    }
    assert_ne!(digest(&reordered_attributes), digest(&records));
}

#[test]
fn produces_the_same_logical_dump_for_the_same_fixture() {
    let first = TempDatabase::new("deterministic-a");
    let second = TempDatabase::new("deterministic-b");
    let mut first_connection = create_new(first.path()).unwrap();
    let mut second_connection = create_new(second.path()).unwrap();
    populate_ready_fixture(&mut first_connection);
    populate_ready_fixture(&mut second_connection);
    let records = lossless_records();
    insert_records(&mut first_connection, &records);
    insert_records(&mut second_connection, &records);

    assert_eq!(
        logical_dump(&first_connection),
        logical_dump(&second_connection)
    );
    drop(first_connection);
    drop(second_connection);
    assert_eq!(
        validate(first.path(), ValidationLevel::SchemaOnly)
            .unwrap()
            .schema_fingerprint,
        validate(second.path(), ValidationLevel::SchemaOnly)
            .unwrap()
            .schema_fingerprint
    );
}

#[test]
fn enforces_foreign_keys_and_strict_types() {
    let fixture = TempDatabase::new("constraints");
    let connection = create_new(fixture.path()).unwrap();
    let foreign_key_error = connection.execute(
        "INSERT INTO source_file( \
             corpus_id, relative_path, dictionary, source_ordinal, volume_number, volume_total \
         ) VALUES ('missing', 'krdict/001.xml', 'krdict', 0, 1, 1)",
        [],
    );
    assert!(foreign_key_error.is_err());

    let type_error = connection.execute(
        "INSERT INTO corpus(corpus_id, source_commit, state, source_file_count, entry_count) \
         VALUES ('bad', ?1, 'importing', 'not-an-integer', NULL)",
        params![COMMIT],
    );
    assert!(type_error.is_err());
}

fn populate_ready_fixture(connection: &mut Connection) {
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO corpus(corpus_id, source_commit, state, source_file_count, entry_count) \
             VALUES (?1, ?1, 'ready', 3, 4)",
            params![COMMIT],
        )
        .unwrap();
    for (ordinal, dictionary) in ["krdict", "stdict", "opendict"].into_iter().enumerate() {
        let path = format!("{dictionary}/001.xml");
        let entry_count = if dictionary == "krdict" { 2 } else { 1 };
        transaction
            .execute(
                "INSERT INTO source_file( \
                     corpus_id, relative_path, dictionary, source_ordinal, volume_number, \
                     volume_total, record_sha256, record_count, entry_count \
                 ) VALUES (?1, ?2, ?3, ?4, 1, 1, ?5, 7, ?6)",
                params![
                    COMMIT,
                    path,
                    dictionary,
                    ordinal as i64,
                    "a".repeat(64),
                    entry_count
                ],
            )
            .unwrap();
    }

    let krdict_entry = entity_id("krdict", "entry/001");
    let krdict_duplicate = entity_id("krdict", "entry/001/at/krdict%3A2");
    let stdict_entry = entity_id("stdict", "entry/001");
    let opendict_entry = entity_id("opendict", "entry/001");
    insert_entity(
        &transaction,
        &krdict_entry,
        "krdict",
        "krdict/001.xml",
        "entry",
        None,
        "krdict:krdict/001.xml#entry=1",
        1,
        0,
    );
    insert_entity(
        &transaction,
        &krdict_duplicate,
        "krdict",
        "krdict/001.xml",
        "entry",
        None,
        "krdict:krdict/001.xml#entry=2",
        2,
        0,
    );
    insert_entity(
        &transaction,
        &stdict_entry,
        "stdict",
        "stdict/001.xml",
        "entry",
        None,
        "stdict:stdict/001.xml#entry=1",
        1,
        0,
    );
    insert_entity(
        &transaction,
        &opendict_entry,
        "opendict",
        "opendict/001.xml",
        "entry",
        None,
        "opendict:opendict/001.xml#entry=1",
        1,
        0,
    );

    for (kind_ordinal, (kind, key)) in [
        ("part_of_speech", "1001"),
        ("common_pattern", "1001001"),
        ("sense", "001"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("{stdict_entry}/{kind}/{key}");
        insert_entity(
            &transaction,
            &id,
            "stdict",
            "stdict/001.xml",
            kind,
            Some(&stdict_entry),
            &format!("stdict:stdict/001.xml#entry=1/{kind}={}", kind_ordinal + 1),
            1,
            (kind_ordinal + 1) as i64,
        );
    }
    let opendict_sense = format!("{opendict_entry}/sense/001");
    insert_entity(
        &transaction,
        &opendict_sense,
        "opendict",
        "opendict/001.xml",
        "sense",
        Some(&opendict_entry),
        "opendict:opendict/001.xml#entry=1/sense=1",
        1,
        1,
    );

    for (entry, headword, homonym) in [
        (&krdict_entry, "가ᄀᆞ", Some("1")),
        (&krdict_duplicate, "가ᄂᆞ", Some("2")),
        (&stdict_entry, "標準", None),
        (&opendict_entry, "열린말", None),
    ] {
        transaction
            .execute(
                "INSERT INTO entry_projection(entry_id, headword, homonym_number, headword_record_ordinal) \
                 VALUES (?1, ?2, ?3, 1)",
                params![entry, headword, homonym],
            )
            .unwrap();
    }
    transaction
        .execute(
            "INSERT INTO text_projection( \
                 entry_id, projection_ordinal, entity_id, field_kind, text_value, source_record_ordinal \
             ) VALUES (?1, 0, ?1, 'definition', '뜻풀이 English 다국어', 2)",
            params![krdict_entry],
        )
        .unwrap();

    for (ordinal, status, target, reason) in [
        (
            0_i64,
            "resolved",
            Some((&stdict_entry, &stdict_entry)),
            "resolved target",
        ),
        (
            1,
            "self_reference",
            Some((&krdict_entry, &krdict_entry)),
            "self target",
        ),
        (2, "unresolved", None, "missing target"),
        (3, "ambiguous", None, "multiple candidates"),
    ] {
        let relation_id = format!("{krdict_entry}/relation/{ordinal}");
        let (target_id, target_entry_id) = target
            .map(|(target_id, target_entry_id)| (Some(target_id), Some(target_entry_id)))
            .unwrap_or((None, None));
        transaction
            .execute(
                "INSERT INTO relation( \
                     relation_id, source_entity_id, source_entry_id, relation_ordinal, \
                     target_namespace, status, resolved_target_id, resolved_target_entry_id, reason \
                 ) VALUES (?1, ?2, ?2, ?3, 'stdict:entry', ?4, ?5, ?6, ?7)",
                params![
                    relation_id,
                    krdict_entry,
                    ordinal,
                    status,
                    target_id,
                    target_entry_id,
                    reason
                ],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO relation_raw_field(relation_id, field_kind, field_ordinal, field_value) \
                 VALUES (?1, 'type', 0, ?2)",
                params![relation_id, status],
            )
            .unwrap();
        if let Some((target_id, _)) = target {
            transaction
                .execute(
                    "INSERT INTO relation_candidate(relation_id, candidate_ordinal, candidate_target_id) \
                     VALUES (?1, 0, ?2)",
                    params![relation_id, target_id],
                )
                .unwrap();
        } else if status == "ambiguous" {
            for (candidate_ordinal, candidate_target_id) in
                [&stdict_entry, &opendict_entry].into_iter().enumerate()
            {
                transaction
                    .execute(
                        "INSERT INTO relation_candidate( \
                             relation_id, candidate_ordinal, candidate_target_id \
                         ) VALUES (?1, ?2, ?3)",
                        params![relation_id, candidate_ordinal as i64, candidate_target_id],
                    )
                    .unwrap();
            }
        }
    }
    transaction.commit().unwrap();
}

#[allow(clippy::too_many_arguments)]
fn insert_entity(
    connection: &Connection,
    id: &str,
    dictionary: &str,
    relative_path: &str,
    kind: &str,
    parent: Option<&str>,
    locator: &str,
    entry_ordinal: i64,
    kind_ordinal: i64,
) {
    connection
        .execute(
            "INSERT INTO entity( \
                 canonical_id, corpus_id, relative_path, dictionary, entity_kind, native_key, \
                 parent_entry_id, source_locator, entry_ordinal, kind_ordinal \
             ) VALUES (?1, ?2, ?3, ?4, ?5, '001', ?6, ?7, ?8, ?9)",
            params![
                id,
                COMMIT,
                relative_path,
                dictionary,
                kind,
                parent,
                locator,
                entry_ordinal,
                kind_ordinal
            ],
        )
        .unwrap();
}

fn entity_id(dictionary: &str, suffix: &str) -> String {
    format!("kweb:v1/{COMMIT}/{dictionary}/{suffix}")
}

fn lossless_records() -> Vec<SourceRecord> {
    vec![
        SourceRecord::StartElement {
            depth: 0,
            name: "future:entry".to_owned(),
            attributes: vec![
                SourceAttribute {
                    name: "z:관리코드".to_owned(),
                    value: "001".to_owned(),
                },
                SourceAttribute {
                    name: "a:lang".to_owned(),
                    value: "ko".to_owned(),
                },
            ],
        },
        SourceRecord::EmptyElement {
            depth: 1,
            name: "future:empty".to_owned(),
            attributes: vec![SourceAttribute {
                name: "url".to_owned(),
                value: format!("https://example.invalid/{}", "긴주소".repeat(128)),
            }],
        },
        SourceRecord::ElementText {
            depth: 1,
            value: "ᄀᆞ multilingual 日本語 English \u{0008}".to_owned(),
        },
        SourceRecord::TailText {
            depth: 0,
            value: " tail 값 ".to_owned(),
        },
        SourceRecord::EndElement {
            depth: 0,
            name: "future:entry".to_owned(),
        },
    ]
}

fn insert_records(connection: &mut Connection, records: &[SourceRecord]) {
    let entry_id = entity_id("krdict", "entry/001");
    let transaction = connection.transaction().unwrap();
    for (record_ordinal, record) in records.iter().enumerate() {
        let (kind, depth, qname, value, attributes) = match record {
            SourceRecord::StartElement {
                depth,
                name,
                attributes,
            } => (
                "start_element",
                *depth,
                Some(name.as_str()),
                None,
                attributes.as_slice(),
            ),
            SourceRecord::EmptyElement {
                depth,
                name,
                attributes,
            } => (
                "empty_element",
                *depth,
                Some(name.as_str()),
                None,
                attributes.as_slice(),
            ),
            SourceRecord::ElementText { depth, value } => {
                ("element_text", *depth, None, Some(value.as_str()), &[][..])
            }
            SourceRecord::TailText { depth, value } => {
                ("tail_text", *depth, None, Some(value.as_str()), &[][..])
            }
            SourceRecord::EndElement { depth, name } => {
                ("end_element", *depth, Some(name.as_str()), None, &[][..])
            }
        };
        transaction
            .execute(
                "INSERT INTO source_record( \
                     corpus_id, relative_path, record_ordinal, entry_id, record_kind, depth, qname, text_value \
                 ) VALUES (?1, 'krdict/001.xml', ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    COMMIT,
                    record_ordinal as i64,
                    entry_id,
                    kind,
                    depth as i64,
                    qname,
                    value
                ],
            )
            .unwrap();
        for (attribute_ordinal, attribute) in attributes.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO source_attribute( \
                         corpus_id, relative_path, record_ordinal, attribute_ordinal, qname, attribute_value \
                     ) VALUES (?1, 'krdict/001.xml', ?2, ?3, ?4, ?5)",
                    params![
                        COMMIT,
                        record_ordinal as i64,
                        attribute_ordinal as i64,
                        attribute.name,
                        attribute.value
                    ],
                )
                .unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn read_records(connection: &Connection) -> Vec<SourceRecord> {
    let mut statement = connection
        .prepare(
            "SELECT record_ordinal, record_kind, depth, qname, text_value \
             FROM source_record \
             WHERE corpus_id = ?1 AND relative_path = 'krdict/001.xml' \
             ORDER BY record_ordinal",
        )
        .unwrap();
    statement
        .query_map(params![COMMIT], |row| {
            let ordinal: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let depth: i64 = row.get(2)?;
            let qname: Option<String> = row.get(3)?;
            let value: Option<String> = row.get(4)?;
            let attributes = if matches!(kind.as_str(), "start_element" | "empty_element") {
                read_attributes(connection, ordinal)
            } else {
                Vec::new()
            };
            let depth = usize::try_from(depth).unwrap();
            Ok(match kind.as_str() {
                "start_element" => SourceRecord::StartElement {
                    depth,
                    name: qname.unwrap(),
                    attributes,
                },
                "empty_element" => SourceRecord::EmptyElement {
                    depth,
                    name: qname.unwrap(),
                    attributes,
                },
                "element_text" => SourceRecord::ElementText {
                    depth,
                    value: value.unwrap(),
                },
                "tail_text" => SourceRecord::TailText {
                    depth,
                    value: value.unwrap(),
                },
                "end_element" => SourceRecord::EndElement {
                    depth,
                    name: qname.unwrap(),
                },
                _ => unreachable!(),
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn read_attributes(connection: &Connection, record_ordinal: i64) -> Vec<SourceAttribute> {
    let mut statement = connection
        .prepare(
            "SELECT qname, attribute_value FROM source_attribute \
             WHERE corpus_id = ?1 AND relative_path = 'krdict/001.xml' AND record_ordinal = ?2 \
             ORDER BY attribute_ordinal",
        )
        .unwrap();
    statement
        .query_map(params![COMMIT, record_ordinal], |row| {
            Ok(SourceAttribute {
                name: row.get(0)?,
                value: row.get(1)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn digest(records: &[SourceRecord]) -> String {
    let mut digest = CanonicalDigest::new();
    for record in records {
        digest.update(record);
    }
    digest.finalize().sha256
}

fn logical_dump(connection: &Connection) -> String {
    let table_orders = [
        ("kweb_metadata", "singleton"),
        ("schema_migration", "version"),
        ("corpus", "corpus_id"),
        ("source_file", "corpus_id, relative_path"),
        ("entity", "canonical_id"),
        ("source_record", "corpus_id, relative_path, record_ordinal"),
        (
            "source_attribute",
            "corpus_id, relative_path, record_ordinal, attribute_ordinal",
        ),
        ("entry_projection", "entry_id"),
        ("text_projection", "entry_id, projection_ordinal"),
        ("relation", "relation_id"),
        (
            "relation_raw_field",
            "relation_id, field_kind, field_ordinal",
        ),
        (
            "relation_candidate",
            "relation_id, candidate_ordinal, candidate_target_id",
        ),
    ];
    let mut output = String::new();
    for (table, order) in table_orders {
        output.push_str(table);
        output.push('\n');
        let sql = format!("SELECT * FROM {table} ORDER BY {order}");
        let mut statement = connection.prepare(&sql).unwrap();
        let column_count = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::with_capacity(column_count);
                for index in 0..column_count {
                    values.push(format_value(row.get_ref(index)?));
                }
                Ok(values.join("|"))
            })
            .unwrap();
        for row in rows {
            output.push_str(&row.unwrap());
            output.push('\n');
        }
    }
    output
}

fn format_value(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "null".to_owned(),
        ValueRef::Integer(value) => format!("i:{value}"),
        ValueRef::Real(value) => format!("r:{value}"),
        ValueRef::Text(value) => format!("t:{}:{}", value.len(), String::from_utf8_lossy(value)),
        ValueRef::Blob(value) => format!("b:{}:{value:?}", value.len()),
    }
}

fn assert_all_application_tables_are_strict_without_rowid(connection: &Connection) {
    let mut statement = connection.prepare("PRAGMA table_list").unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .unwrap();
    let application_tables = rows
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter(|(name, kind, _, _)| kind == "table" && !name.starts_with("sqlite_"))
        .collect::<Vec<_>>();
    assert_eq!(application_tables.len(), 12);
    assert!(
        application_tables
            .iter()
            .all(|(_, _, without_rowid, strict)| *without_rowid == 1 && *strict == 1)
    );
}

fn pragma_i64(connection: &Connection, name: &str) -> i64 {
    connection
        .pragma_query_value(None, name, |row| row.get(0))
        .unwrap()
}
