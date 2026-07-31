use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::catalog::Dictionary;
use korean_dict_epub::record::{CanonicalDigest, SourceAttribute, SourceRecord};
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

fn source_file(source_ordinal: u64, relative_path: &str) -> SourceFileIdentity {
    SourceFileIdentity {
        relative_path: PathBuf::from(relative_path),
        dictionary: Dictionary::Krdict,
        source_ordinal,
        volume_number: source_ordinal + 1,
        volume_total: 3,
    }
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
