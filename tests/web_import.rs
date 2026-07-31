use std::fs::{self, File};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::catalog::Dictionary;
use korean_dict_epub::record::{CanonicalDigest, SourceAttribute, SourceRecord};
use korean_dict_epub::source::SourceRecordReader;
use korean_dict_epub::web_db::{ValidationLevel, validate};
use korean_dict_epub::web_import::{
    ExpectedCorpus, FileTransactionOutcome, ImportError, ImportMode, SourceFileCompletion,
    SourceFileIdentity, begin_import,
};
use rusqlite::{Connection, params};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const COMMIT: &str = "42c0d01889f34536e9cf94fe57f62bd2055b1bde";
const OTHER_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "korean-dict-web-import-{label}-{}-{sequence}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("dictionary.sqlite")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn new_rejects_existing_database_sidecars_and_unsafe_targets_without_changes() {
    let existing = TempRoot::new("new-existing");
    let database = existing.database();
    fs::write(&database, b"keep-existing").unwrap();
    let error = begin_import(&database, COMMIT, ImportMode::New).unwrap_err();
    assert!(matches!(error, ImportError::InvalidLifecycle { .. }));
    assert_eq!(fs::read(&database).unwrap(), b"keep-existing");

    for suffix in ["-journal", "-wal", "-shm"] {
        let fixture = TempRoot::new(&format!("new{suffix}"));
        let database = fixture.database();
        let sidecar = append_suffix(&database, suffix);
        fs::write(&sidecar, b"keep-sidecar").unwrap();
        let error = begin_import(&database, COMMIT, ImportMode::New).unwrap_err();
        assert!(matches!(error, ImportError::InvalidLifecycle { .. }));
        assert!(!database.exists());
        assert_eq!(fs::read(sidecar).unwrap(), b"keep-sidecar");
    }

    let directory = TempRoot::new("new-directory");
    let error = begin_import(&directory.path, COMMIT, ImportMode::New).unwrap_err();
    assert!(matches!(error, ImportError::InvalidLifecycle { .. }));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let fixture = TempRoot::new("new-symlink");
        let target = fixture.path.join("target.sqlite");
        let link = fixture.database();
        fs::write(&target, b"target").unwrap();
        symlink(&target, &link).unwrap();
        let error = begin_import(&link, COMMIT, ImportMode::New).unwrap_err();
        assert!(matches!(error, ImportError::InvalidLifecycle { .. }));
        assert_eq!(fs::read(target).unwrap(), b"target");
    }
}

#[test]
fn file_transactions_commit_once_and_rollback_errors_and_panics() {
    let fixture = TempRoot::new("file-transactions");
    let database = fixture.database();
    let active_setting = fixture.path.join("active-db-setting.json");
    fs::write(&active_setting, b"keep-active-setting").unwrap();
    let first = source_file(0, "krdict/001.xml");
    let second = source_file(1, "krdict/002.xml");
    let third = source_file(2, "krdict/003.xml");
    let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();

    let outcome = session
        .with_file_transaction(&first, |transaction| {
            insert_entry(transaction, &first, "001", true);
            Ok(completion())
        })
        .unwrap();
    assert!(matches!(outcome, FileTransactionOutcome::Committed(_)));

    let mut duplicate_callback_ran = false;
    let outcome = session
        .with_file_transaction(&first, |_| {
            duplicate_callback_ran = true;
            Ok(completion())
        })
        .unwrap();
    assert!(matches!(
        outcome,
        FileTransactionOutcome::AlreadyComplete(_)
    ));
    assert!(!duplicate_callback_ran);
    let mut mismatched = first.clone();
    mismatched.volume_total += 1;
    assert!(
        session
            .with_file_transaction(&mismatched, |_| Ok(completion()))
            .is_err()
    );

    let error = session
        .with_file_transaction(&second, |transaction| {
            insert_entry(transaction, &second, "002", true);
            Err(ImportError::Operation("injected failure".to_owned()))
        })
        .unwrap_err();
    assert!(matches!(error, ImportError::Operation(_)));

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = session.with_file_transaction(&third, |transaction| {
            insert_entry(transaction, &third, "003", true);
            panic!("injected interruption");
        });
    }));
    assert!(panic.is_err());
    drop(session);
    assert_eq!(fs::read(&active_setting).unwrap(), b"keep-active-setting");

    let mut resumed = begin_import(&database, COMMIT, ImportMode::Resume).unwrap();
    for (identity, native_key) in [(&second, "002"), (&third, "003")] {
        let outcome = resumed
            .with_file_transaction(identity, |transaction| {
                insert_entry(transaction, identity, native_key, true);
                Ok(completion())
            })
            .unwrap();
        assert!(matches!(outcome, FileTransactionOutcome::Committed(_)));
    }

    let connection = Connection::open(&database).unwrap();
    let files: i64 = connection
        .query_row("SELECT count(*) FROM source_file", [], |row| row.get(0))
        .unwrap();
    let entries: i64 = connection
        .query_row(
            "SELECT count(*) FROM entity WHERE entity_kind = 'entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((files, entries), (3, 3));
    assert_eq!(fs::read(&active_setting).unwrap(), b"keep-active-setting");
}

#[test]
fn resume_requires_exact_schema_commit_state_and_complete_files() {
    let valid = TempRoot::new("resume-valid");
    let database = valid.database();
    drop(begin_import(&database, COMMIT, ImportMode::New).unwrap());
    drop(begin_import(&database, COMMIT, ImportMode::Resume).unwrap());
    assert!(begin_import(&database, OTHER_COMMIT, ImportMode::Resume).is_err());

    for (label, mutation) in [
        (
            "ready",
            "UPDATE corpus SET state = 'ready', source_file_count = 0, entry_count = 0",
        ),
        ("failed", "UPDATE corpus SET state = 'failed'"),
        ("future", "PRAGMA user_version = 2"),
        ("drift", "CREATE INDEX unexpected_index ON corpus(state)"),
    ] {
        let fixture = TempRoot::new(&format!("resume-{label}"));
        let database = fixture.database();
        drop(begin_import(&database, COMMIT, ImportMode::New).unwrap());
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch(mutation).unwrap();
        drop(connection);
        assert!(
            begin_import(&database, COMMIT, ImportMode::Resume).is_err(),
            "resume must reject {label}"
        );
    }

    let partial = TempRoot::new("resume-partial");
    let database = partial.database();
    drop(begin_import(&database, COMMIT, ImportMode::New).unwrap());
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO source_file( \
                 corpus_id, relative_path, dictionary, source_ordinal, volume_number, volume_total \
             ) VALUES (?1, 'krdict/001.xml', 'krdict', 0, 1, 1)",
            params![COMMIT],
        )
        .unwrap();
    drop(connection);
    assert!(begin_import(&database, COMMIT, ImportMode::Resume).is_err());
}

#[test]
fn finalization_commits_ready_only_after_ready_validation_passes() {
    let invalid = TempRoot::new("finalize-rollback");
    let database = invalid.database();
    let identity = source_file(0, "krdict/001.xml");
    let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
    session
        .with_file_transaction(&identity, |transaction| {
            insert_entry(transaction, &identity, "001", false);
            Ok(completion())
        })
        .unwrap();
    let expected = expected(vec![identity.clone()]);
    assert!(session.finalize(&expected).is_err());

    let connection = Connection::open(&database).unwrap();
    let state: (String, Option<i64>, Option<i64>) = connection
        .query_row(
            "SELECT state, source_file_count, entry_count FROM corpus",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, ("importing".to_owned(), None, None));
    drop(connection);

    let digest_drift = TempRoot::new("finalize-record-drift");
    let database = digest_drift.database();
    let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
    session
        .with_file_transaction(&identity, |transaction| {
            insert_entry(transaction, &identity, "001", true);
            Ok(completion())
        })
        .unwrap();
    drop(session);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE source_attribute SET attribute_value = 'mutated'",
            [],
        )
        .unwrap();
    drop(connection);
    let session = begin_import(&database, COMMIT, ImportMode::Resume).unwrap();
    assert!(session.finalize(&expected).is_err());
    let connection = Connection::open(&database).unwrap();
    let state: String = connection
        .query_row("SELECT state FROM corpus", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "importing");
    drop(connection);

    let valid = TempRoot::new("finalize-ready");
    let database = valid.database();
    let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
    session
        .with_file_transaction(&identity, |transaction| {
            insert_entry(transaction, &identity, "001", true);
            Ok(completion())
        })
        .unwrap();
    let descriptor = session.finalize(&expected).unwrap();
    assert_eq!(descriptor.corpus.unwrap().entry_count, 1);
    validate(&database, ValidationLevel::ReadyCorpus).unwrap();
}

#[test]
fn rebuild_preserves_the_target_until_a_valid_staging_database_is_ready() {
    let failed = TempRoot::new("rebuild-failed");
    let database = failed.database();
    let active_setting = failed.path.join("active-db-setting.json");
    fs::write(&database, b"old-database").unwrap();
    fs::write(&active_setting, b"keep-active-setting").unwrap();
    let identity = source_file(0, "krdict/001.xml");
    let mut session = begin_import(&database, COMMIT, ImportMode::Rebuild).unwrap();
    let staging = session.working_path().to_path_buf();
    session
        .with_file_transaction(&identity, |transaction| {
            insert_entry(transaction, &identity, "001", false);
            Ok(completion())
        })
        .unwrap();
    assert!(session.finalize(&expected(vec![identity.clone()])).is_err());
    assert_eq!(fs::read(&database).unwrap(), b"old-database");
    assert_eq!(fs::read(&active_setting).unwrap(), b"keep-active-setting");
    assert!(!staging.exists());

    let succeeded = TempRoot::new("rebuild-succeeded");
    let database = succeeded.database();
    let active_setting = succeeded.path.join("active-db-setting.json");
    fs::write(&database, b"old-database").unwrap();
    fs::write(&active_setting, b"keep-active-setting").unwrap();
    let mut session = begin_import(&database, COMMIT, ImportMode::Rebuild).unwrap();
    session
        .with_file_transaction(&identity, |transaction| {
            insert_entry(transaction, &identity, "001", true);
            Ok(completion())
        })
        .unwrap();
    session.finalize(&expected(vec![identity])).unwrap();
    validate(&database, ValidationLevel::ReadyCorpus).unwrap();
    assert_ne!(fs::read(&database).unwrap(), b"old-database");
    assert_eq!(fs::read(&active_setting).unwrap(), b"keep-active-setting");
}

#[test]
fn rebuild_rejects_sidecars_at_start_and_before_publish() {
    let start = TempRoot::new("rebuild-sidecar-start");
    let database = start.database();
    fs::write(&database, b"old-database").unwrap();
    let sidecar = append_suffix(&database, "-wal");
    fs::write(&sidecar, b"keep-sidecar").unwrap();
    assert!(begin_import(&database, COMMIT, ImportMode::Rebuild).is_err());
    assert_eq!(fs::read(&database).unwrap(), b"old-database");
    assert_eq!(fs::read(&sidecar).unwrap(), b"keep-sidecar");

    let late = TempRoot::new("rebuild-sidecar-late");
    let database = late.database();
    fs::write(&database, b"old-database").unwrap();
    let identity = source_file(0, "krdict/001.xml");
    let mut session = begin_import(&database, COMMIT, ImportMode::Rebuild).unwrap();
    session
        .with_file_transaction(&identity, |transaction| {
            insert_entry(transaction, &identity, "001", true);
            Ok(completion())
        })
        .unwrap();
    let sidecar = append_suffix(&database, "-journal");
    fs::write(&sidecar, b"appeared-late").unwrap();
    assert!(session.finalize(&expected(vec![identity])).is_err());
    assert_eq!(fs::read(&database).unwrap(), b"old-database");
    assert_eq!(fs::read(&sidecar).unwrap(), b"appeared-late");
}

#[test]
fn streams_three_dictionary_fixtures_into_one_ready_mini_corpus() {
    let fixture = TempRoot::new("three-dictionary-mini-corpus");
    let database = fixture.database();
    let source_root = fixture.path.join("source");
    let specs = [
        (Dictionary::Krdict, "krdict.xml", "krdict/fixture.xml"),
        (Dictionary::Stdict, "stdict.xml", "stdict/fixture.xml"),
        (Dictionary::Opendict, "opendict.xml", "opendict/fixture.xml"),
    ];
    let mut identities = Vec::new();
    let mut source_paths = Vec::new();
    for (source_ordinal, (dictionary, fixture_name, relative_path)) in specs.into_iter().enumerate()
    {
        let source_path = source_root.join(dictionary.key()).join("fixture.xml");
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        let mut bytes = fs::read(source_fixture(fixture_name)).unwrap();
        if dictionary == Dictionary::Krdict {
            bytes = replace_bytes(&bytes, b"CONTROL_BYTE", &[0x08]);
        }
        fs::write(&source_path, bytes).unwrap();
        identities.push(SourceFileIdentity {
            relative_path: PathBuf::from(relative_path),
            dictionary,
            source_ordinal: u64::try_from(source_ordinal).unwrap(),
            volume_number: 1,
            volume_total: 1,
        });
        source_paths.push(source_path);
    }

    let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
    for (identity, source_path) in identities.iter().zip(&source_paths) {
        let outcome = session
            .import_source_file(identity, source_path)
            .expect("fixture import should commit");
        assert!(matches!(outcome, FileTransactionOutcome::Committed(_)));
    }
    let descriptor = session
        .finalize(&ExpectedCorpus {
            source_commit: COMMIT.to_owned(),
            source_files: identities.clone(),
        })
        .unwrap();
    let corpus = descriptor.corpus.unwrap();
    assert_eq!((corpus.source_file_count, corpus.entry_count), (3, 6));
    validate(&database, ValidationLevel::ReadyCorpus).unwrap();

    let connection = Connection::open(&database).unwrap();
    for (identity, source_path) in identities.iter().zip(&source_paths) {
        let expected = source_digest(source_path);
        let actual: (String, i64, i64) = connection
            .query_row(
                "SELECT record_sha256, record_count, entry_count FROM source_file \
                 WHERE corpus_id = ?1 AND relative_path = ?2",
                params![COMMIT, identity.relative_path.to_string_lossy()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(actual.0, expected.0);
        assert_eq!(u64::try_from(actual.1).unwrap(), expected.1);
        assert_eq!(actual.2, 2);
    }

    let entries = connection
        .prepare(
            "SELECT entity.canonical_id, entity.native_key, entry_projection.headword, \
                    entry_projection.homonym_number, entity.entry_ordinal, \
                    entry_projection.headword_record_ordinal \
             FROM source_file \
             JOIN entity ON entity.corpus_id = source_file.corpus_id \
                        AND entity.relative_path = source_file.relative_path \
             JOIN entry_projection ON entry_projection.entry_id = entity.canonical_id \
             WHERE entity.entity_kind = 'entry' \
             ORDER BY source_file.source_ordinal, entity.entry_ordinal",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (&entry.1, &entry.2, entry.3.as_deref(), entry.4))
            .collect::<Vec<_>>(),
        vec![
            (&"0001".to_owned(), &"ᄔᅡ라".to_owned(), Some("1"), 1),
            (&"0002".to_owned(), &"가상 표제어".to_owned(), Some("2"), 2),
            (&"0010".to_owned(), &"가상 표제어".to_owned(), None, 1),
            (&"0020".to_owned(), &"두 번째 표제어".to_owned(), None, 2),
            (&"0100".to_owned(), &"합성 표제어".to_owned(), Some("1"), 1),
            (
                &"0200".to_owned(),
                &"두 번째 열린 표제어".to_owned(),
                Some("2"),
                2
            ),
        ]
    );
    assert!(entries.iter().all(|entry| entry.5.is_some()));
    assert_eq!(entries[0].0, format!("kweb:v1/{COMMIT}/krdict/entry/0001"));

    let opaque_attributes = connection
        .prepare(
            "SELECT source_attribute.qname FROM source_record \
             JOIN source_attribute USING(corpus_id, relative_path, record_ordinal) \
             WHERE source_record.qname = 'future:opaque' \
             ORDER BY source_attribute.attribute_ordinal",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(opaque_attributes, ["zeta", "alpha"]);
    let text_values = connection
        .prepare("SELECT text_value FROM source_record WHERE text_value IS NOT NULL")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(text_values.iter().any(|value| value.contains('\u{0008}')));
    assert!(text_values.iter().any(|value| value == "仮の見出し語"));
    assert!(text_values.iter().any(|value| value == "뒤"));
    let long_url: String = connection
        .query_row(
            "SELECT attribute_value FROM source_attribute WHERE qname = 'url'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(long_url.starts_with("https://example.invalid/"));
}

#[test]
fn rejects_invalid_entry_shapes_and_rolls_back_the_file() {
    let cases = [
        (
            "missing-key",
            "<LexicalResource><LexicalEntry><Lemma><feat att=\"writtenForm\" val=\"표제어\"/></Lemma></LexicalEntry></LexicalResource>",
        ),
        (
            "empty-headword",
            "<LexicalResource><LexicalEntry id=\"1\"><Lemma><feat att=\"writtenForm\" val=\" \"/></Lemma></LexicalEntry></LexicalResource>",
        ),
        (
            "duplicate-key",
            "<LexicalResource><LexicalEntry id=\"1\"><Lemma><feat att=\"writtenForm\" val=\"하나\"/></Lemma></LexicalEntry><LexicalEntry id=\"1\"><Lemma><feat att=\"writtenForm\" val=\"둘\"/></Lemma></LexicalEntry></LexicalResource>",
        ),
        (
            "nested-entry",
            "<LexicalResource><LexicalEntry id=\"1\"><LexicalEntry id=\"2\"/></LexicalEntry></LexicalResource>",
        ),
        (
            "malformed",
            "<LexicalResource><LexicalEntry id=\"1\"><Lemma></LexicalResource>",
        ),
    ];
    for (label, xml) in cases {
        let fixture = TempRoot::new(label);
        let database = fixture.database();
        let source = fixture.path.join("source.xml");
        fs::write(&source, xml).unwrap();
        let identity = source_file(0, "krdict/fixture.xml");
        let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
        assert!(
            session.import_source_file(&identity, &source).is_err(),
            "{label} should fail"
        );
        drop(session);
        let connection = Connection::open(&database).unwrap();
        let counts: (i64, i64, i64) = connection
            .query_row(
                "SELECT \
                     (SELECT count(*) FROM source_file), \
                     (SELECT count(*) FROM source_record), \
                     (SELECT count(*) FROM entity)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0, 0), "{label} must roll back");
    }
}

fn source_file(source_ordinal: u64, relative_path: &str) -> SourceFileIdentity {
    SourceFileIdentity {
        relative_path: PathBuf::from(relative_path),
        dictionary: Dictionary::Krdict,
        source_ordinal,
        volume_number: source_ordinal + 1,
        volume_total: 3,
    }
}

fn source_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/source")
        .join(name)
}

fn source_digest(path: &Path) -> (String, u64) {
    let mut digest = CanonicalDigest::new();
    let mut count = 0;
    for record in SourceRecordReader::new(File::open(path).unwrap()) {
        digest.update(&record.unwrap());
        count += 1;
    }
    (digest.finalize().sha256, count)
}

fn replace_bytes(input: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let offset = input
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("fixture marker should exist");
    let mut output = Vec::with_capacity(input.len() - needle.len() + replacement.len());
    output.extend_from_slice(&input[..offset]);
    output.extend_from_slice(replacement);
    output.extend_from_slice(&input[offset + needle.len()..]);
    output
}

fn completion() -> SourceFileCompletion {
    let mut digest = CanonicalDigest::new();
    digest.update(&SourceRecord::EmptyElement {
        depth: 0,
        name: "entry".to_owned(),
        attributes: vec![SourceAttribute {
            name: "native-key".to_owned(),
            value: "preserved".to_owned(),
        }],
    });
    SourceFileCompletion {
        record_sha256: digest.finalize().sha256,
        record_count: 1,
        entry_count: 1,
    }
}

fn expected(source_files: Vec<SourceFileIdentity>) -> ExpectedCorpus {
    ExpectedCorpus {
        source_commit: COMMIT.to_owned(),
        source_files,
    }
}

fn insert_entry(
    transaction: &rusqlite::Transaction<'_>,
    identity: &SourceFileIdentity,
    native_key: &str,
    include_projection: bool,
) {
    let relative_path = identity.relative_path.to_string_lossy().replace('\\', "/");
    let entry_id = format!("kweb:v1/{COMMIT}/krdict/entry/{native_key}");
    transaction
        .execute(
            "INSERT INTO entity( \
                 canonical_id, corpus_id, relative_path, dictionary, entity_kind, native_key, \
                 parent_entry_id, source_locator, entry_ordinal, kind_ordinal \
             ) VALUES (?1, ?2, ?3, 'krdict', 'entry', ?4, NULL, ?5, 0, 0)",
            params![
                entry_id,
                COMMIT,
                relative_path,
                native_key,
                format!("krdict:{relative_path}#entry=0")
            ],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO source_record( \
                 corpus_id, relative_path, record_ordinal, entry_id, record_kind, depth, qname \
             ) VALUES (?1, ?2, 0, ?3, 'empty_element', 0, 'entry')",
            params![COMMIT, relative_path, entry_id],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO source_attribute( \
                 corpus_id, relative_path, record_ordinal, attribute_ordinal, qname, attribute_value \
             ) VALUES (?1, ?2, 0, 0, 'native-key', 'preserved')",
            params![COMMIT, relative_path],
        )
        .unwrap();
    if include_projection {
        transaction
            .execute(
                "INSERT INTO entry_projection(entry_id, headword, headword_record_ordinal) \
                 VALUES (?1, ?2, 0)",
                params![entry_id, format!("표제어-{native_key}")],
            )
            .unwrap();
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}
