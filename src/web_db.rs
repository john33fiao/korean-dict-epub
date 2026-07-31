use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};

use crate::record::DIGEST_SCHEMA;
pub use crate::web_identity::CANONICAL_ID_SCHEMA;

pub const APPLICATION_ID: i32 = 0x4B57_4542;
pub const LATEST_SCHEMA_VERSION: i64 = 1;
pub const FORMAT_MARKER: &str = "korean-dict-web-db";
pub const RECORD_SCHEMA_MARKER: &str = DIGEST_SCHEMA;
pub const DERIVED_TABLE_PREFIX: &str = "derived_";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const V1_NAME: &str = "sqlite-schema-v1";
const V1_SQL: &str = include_str!("web_db/migrations/0001.sql");

const REQUIRED_TABLES: [&str; 12] = [
    "corpus",
    "entity",
    "entry_projection",
    "kweb_metadata",
    "relation",
    "relation_candidate",
    "relation_raw_field",
    "schema_migration",
    "source_attribute",
    "source_file",
    "source_record",
    "text_projection",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    SchemaOnly,
    ReadyCorpus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDescriptor {
    pub path: PathBuf,
    pub format_marker: String,
    pub schema_version: i64,
    pub schema_fingerprint: String,
    pub canonical_id_schema: String,
    pub record_schema: String,
    pub corpus: Option<CorpusDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusDescriptor {
    pub corpus_id: String,
    pub source_commit: String,
    pub source_file_count: u64,
    pub entry_count: u64,
    pub representative_entry_id: String,
    pub representative_headword: String,
}

#[derive(Debug)]
pub enum WebDbError {
    ExistingPath(PathBuf),
    InvalidPath {
        path: PathBuf,
        reason: String,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
    WrongApplicationId {
        path: PathBuf,
        found: i64,
    },
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    MigrationMismatch {
        path: PathBuf,
        version: i64,
        reason: String,
    },
    InvalidDatabase {
        path: PathBuf,
        reason: String,
    },
}

impl fmt::Display for WebDbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExistingPath(path) => write!(
                formatter,
                "database '{}' already exists; creation never overwrites files",
                path.display()
            ),
            Self::InvalidPath { path, reason } => {
                write!(
                    formatter,
                    "invalid database path '{}': {reason}",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "I/O error for database '{}': {source}",
                    path.display()
                )
            }
            Self::Sqlite { path, source } => {
                write!(formatter, "SQLite error for '{}': {source}", path.display())
            }
            Self::WrongApplicationId { path, found } => write!(
                formatter,
                "database '{}' has application_id {found:#010x}, expected {:#010x}",
                path.display(),
                APPLICATION_ID
            ),
            Self::UnsupportedSchemaVersion {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "database '{}' has schema version {found}, but this build supports up to {supported}",
                path.display()
            ),
            Self::MigrationMismatch {
                path,
                version,
                reason,
            } => write!(
                formatter,
                "database '{}' migration {version} does not match this build: {reason}",
                path.display()
            ),
            Self::InvalidDatabase { path, reason } => {
                write!(
                    formatter,
                    "invalid web dictionary database '{}': {reason}",
                    path.display()
                )
            }
        }
    }
}

impl Error for WebDbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            Self::ExistingPath(_)
            | Self::InvalidPath { .. }
            | Self::WrongApplicationId { .. }
            | Self::UnsupportedSchemaVersion { .. }
            | Self::MigrationMismatch { .. }
            | Self::InvalidDatabase { .. } => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: [Migration; 1] = [Migration {
    version: 1,
    name: V1_NAME,
    sql: V1_SQL,
}];

pub fn create_new(path: &Path) -> Result<Connection, WebDbError> {
    let path = resolve_new_path(path)?;
    reserve_new_database(&path)?;
    let result = (|| {
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|source| sqlite_error(&path, source))?;
        configure_connection(&connection, &path)?;
        migrate(&mut connection, &path)?;
        validate_connection(&connection, &path, ValidationLevel::SchemaOnly)?;
        Ok(connection)
    })();

    if result.is_err() {
        cleanup_new_database(&path);
    }
    result
}

pub fn open_and_migrate(path: &Path) -> Result<Connection, WebDbError> {
    let path = resolve_existing_path(path)?;
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|source| sqlite_error(&path, source))?;
    configure_connection(&connection, &path)?;
    migrate(&mut connection, &path)?;
    validate_connection(&connection, &path, ValidationLevel::SchemaOnly)?;
    Ok(connection)
}

pub fn validate(path: &Path, level: ValidationLevel) -> Result<DatabaseDescriptor, WebDbError> {
    let path = resolve_existing_path(path)?;
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|source| sqlite_error(&path, source))?;
    configure_connection(&connection, &path)?;
    validate_connection(&connection, &path, level)
}

fn configure_connection(connection: &Connection, path: &Path) -> Result<(), WebDbError> {
    connection
        .pragma_update(None, "trusted_schema", false)
        .map_err(|source| sqlite_error(path, source))?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|source| sqlite_error(path, source))?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|source| sqlite_error(path, source))?;
    Ok(())
}

fn migrate(connection: &mut Connection, path: &Path) -> Result<(), WebDbError> {
    let application_id = pragma_i64(connection, "application_id", path)?;
    let current_version = pragma_i64(connection, "user_version", path)?;

    if current_version > LATEST_SCHEMA_VERSION {
        return Err(WebDbError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            found: current_version,
            supported: LATEST_SCHEMA_VERSION,
        });
    }

    if application_id == 0 {
        if current_version != 0 || !database_has_no_application_schema(connection, path)? {
            return Err(WebDbError::WrongApplicationId {
                path: path.to_path_buf(),
                found: application_id,
            });
        }
    } else if application_id != i64::from(APPLICATION_ID) {
        return Err(WebDbError::WrongApplicationId {
            path: path.to_path_buf(),
            found: application_id,
        });
    }

    if current_version > 0 {
        verify_migration_history(connection, path, current_version)?;
    }

    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current_version)
    {
        apply_migration(connection, path, *migration)?;
    }
    Ok(())
}

fn apply_migration(
    connection: &mut Connection,
    path: &Path,
    migration: Migration,
) -> Result<(), WebDbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| sqlite_error(path, source))?;
    apply_migration_in_transaction(&transaction, path, migration)?;
    transaction
        .commit()
        .map_err(|source| sqlite_error(path, source))
}

fn apply_migration_in_transaction(
    transaction: &Transaction<'_>,
    path: &Path,
    migration: Migration,
) -> Result<(), WebDbError> {
    transaction
        .execute_batch(migration.sql)
        .map_err(|source| sqlite_error(path, source))?;
    let checksum = migration_checksum(migration.sql);
    transaction
        .execute(
            "INSERT INTO schema_migration(version, name, sha256) VALUES (?1, ?2, ?3)",
            params![migration.version, migration.name, checksum],
        )
        .map_err(|source| sqlite_error(path, source))?;

    let fingerprint = schema_fingerprint(transaction, path)?;
    transaction
        .execute(
            "UPDATE kweb_metadata SET schema_fingerprint = ?1 WHERE singleton = 1",
            params![fingerprint],
        )
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .pragma_update(None, "application_id", APPLICATION_ID)
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .pragma_update(None, "user_version", migration.version)
        .map_err(|source| sqlite_error(path, source))?;
    Ok(())
}

fn verify_migration_history(
    connection: &Connection,
    path: &Path,
    current_version: i64,
) -> Result<(), WebDbError> {
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= current_version)
    {
        let stored = connection
            .query_row(
                "SELECT name, sha256 FROM schema_migration WHERE version = ?1",
                params![migration.version],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|source| sqlite_error(path, source))?;
        let Some((name, checksum)) = stored else {
            return Err(WebDbError::MigrationMismatch {
                path: path.to_path_buf(),
                version: migration.version,
                reason: "history row is missing".to_owned(),
            });
        };
        let expected_checksum = migration_checksum(migration.sql);
        if name != migration.name || checksum != expected_checksum {
            return Err(WebDbError::MigrationMismatch {
                path: path.to_path_buf(),
                version: migration.version,
                reason: format!(
                    "stored name/checksum is {name}/{checksum}, expected {}/{expected_checksum}",
                    migration.name
                ),
            });
        }
    }
    Ok(())
}

fn validate_connection(
    connection: &Connection,
    path: &Path,
    level: ValidationLevel,
) -> Result<DatabaseDescriptor, WebDbError> {
    let application_id = pragma_i64(connection, "application_id", path)?;
    if application_id != i64::from(APPLICATION_ID) {
        return Err(WebDbError::WrongApplicationId {
            path: path.to_path_buf(),
            found: application_id,
        });
    }
    let schema_version = pragma_i64(connection, "user_version", path)?;
    if schema_version > LATEST_SCHEMA_VERSION {
        return Err(WebDbError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            found: schema_version,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    if schema_version != LATEST_SCHEMA_VERSION {
        return invalid_database(
            path,
            format!(
                "schema version {schema_version} is not the required version {LATEST_SCHEMA_VERSION}"
            ),
        );
    }
    verify_migration_history(connection, path, schema_version)?;
    verify_required_tables(connection, path)?;
    verify_integrity(connection, path)?;

    let metadata = connection
        .query_row(
            "SELECT format_marker, schema_version, schema_fingerprint, \
                    canonical_id_schema, record_schema, derived_table_prefix \
             FROM kweb_metadata WHERE singleton = 1",
            [],
            |row| {
                Ok(MetadataRow {
                    format_marker: row.get(0)?,
                    schema_version: row.get(1)?,
                    schema_fingerprint: row.get(2)?,
                    canonical_id_schema: row.get(3)?,
                    record_schema: row.get(4)?,
                    derived_table_prefix: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(|source| sqlite_error(path, source))?
        .ok_or_else(|| WebDbError::InvalidDatabase {
            path: path.to_path_buf(),
            reason: "metadata singleton is missing".to_owned(),
        })?;
    verify_metadata(path, schema_version, &metadata)?;

    let actual_fingerprint = schema_fingerprint(connection, path)?;
    let expected_fingerprint = expected_schema_fingerprint(path)?;
    if metadata.schema_fingerprint != actual_fingerprint
        || actual_fingerprint != expected_fingerprint
    {
        return invalid_database(
            path,
            format!(
                "schema fingerprint mismatch: stored={}, actual={}, expected={expected_fingerprint}",
                metadata.schema_fingerprint, actual_fingerprint
            ),
        );
    }

    let corpus = match level {
        ValidationLevel::SchemaOnly => None,
        ValidationLevel::ReadyCorpus => Some(validate_ready_corpus(connection, path)?),
    };
    Ok(DatabaseDescriptor {
        path: path.to_path_buf(),
        format_marker: metadata.format_marker,
        schema_version,
        schema_fingerprint: actual_fingerprint,
        canonical_id_schema: metadata.canonical_id_schema,
        record_schema: metadata.record_schema,
        corpus,
    })
}

struct MetadataRow {
    format_marker: String,
    schema_version: i64,
    schema_fingerprint: String,
    canonical_id_schema: String,
    record_schema: String,
    derived_table_prefix: String,
}

fn verify_metadata(
    path: &Path,
    schema_version: i64,
    metadata: &MetadataRow,
) -> Result<(), WebDbError> {
    let valid = metadata.format_marker == FORMAT_MARKER
        && metadata.schema_version == schema_version
        && metadata.canonical_id_schema == CANONICAL_ID_SCHEMA
        && metadata.record_schema == RECORD_SCHEMA_MARKER
        && metadata.derived_table_prefix == DERIVED_TABLE_PREFIX;
    if valid {
        Ok(())
    } else {
        invalid_database(path, "metadata markers do not match this build".to_owned())
    }
}

fn validate_ready_corpus(
    connection: &Connection,
    path: &Path,
) -> Result<CorpusDescriptor, WebDbError> {
    let corpus_count: i64 = connection
        .query_row("SELECT count(*) FROM corpus", [], |row| row.get(0))
        .map_err(|source| sqlite_error(path, source))?;
    if corpus_count != 1 {
        return invalid_database(
            path,
            format!("ready database must contain exactly one corpus, found {corpus_count}"),
        );
    }

    let (corpus_id, source_commit, state, expected_files, expected_entries) = connection
        .query_row(
            "SELECT corpus_id, source_commit, state, source_file_count, entry_count FROM corpus",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .map_err(|source| sqlite_error(path, source))?;
    if state != "ready" || !is_lower_hex_commit(&source_commit) {
        return invalid_database(
            path,
            "corpus is not ready or has an invalid commit".to_owned(),
        );
    }
    let expected_files = expected_files
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| WebDbError::InvalidDatabase {
            path: path.to_path_buf(),
            reason: "ready corpus has no valid source file count".to_owned(),
        })?;
    let expected_entries = expected_entries
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| WebDbError::InvalidDatabase {
            path: path.to_path_buf(),
            reason: "ready corpus has no valid entry count".to_owned(),
        })?;

    let actual_files = count_query(
        connection,
        path,
        "SELECT count(*) FROM source_file WHERE corpus_id = ?1",
        &corpus_id,
    )?;
    let actual_entries = count_query(
        connection,
        path,
        "SELECT count(*) FROM entity WHERE corpus_id = ?1 AND entity_kind = 'entry'",
        &corpus_id,
    )?;
    if actual_files != expected_files || actual_entries != expected_entries {
        return invalid_database(
            path,
            format!(
                "corpus counts differ: files {actual_files}/{expected_files}, entries {actual_entries}/{expected_entries}"
            ),
        );
    }

    let incomplete_files = count_query(
        connection,
        path,
        "SELECT count(*) FROM source_file \
         WHERE corpus_id = ?1 \
           AND (record_sha256 IS NULL OR record_count IS NULL OR entry_count IS NULL)",
        &corpus_id,
    )?;
    if incomplete_files != 0 {
        return invalid_database(
            path,
            format!("ready corpus has {incomplete_files} incomplete source files"),
        );
    }

    verify_ready_references(connection, path, &corpus_id, &source_commit)?;

    let representative = connection
        .query_row(
            "SELECT entity.canonical_id, entry_projection.headword \
             FROM source_file \
             JOIN entity ON entity.corpus_id = source_file.corpus_id \
                        AND entity.relative_path = source_file.relative_path \
             JOIN entry_projection ON entry_projection.entry_id = entity.canonical_id \
             WHERE source_file.corpus_id = ?1 AND entity.entity_kind = 'entry' \
             ORDER BY source_file.source_ordinal, entity.entry_ordinal, entity.canonical_id \
             LIMIT 1",
            params![corpus_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|source| sqlite_error(path, source))?;
    let Some((representative_entry_id, Some(representative_headword))) = representative else {
        return invalid_database(
            path,
            "representative entry lookup returned no headword".to_owned(),
        );
    };
    if representative_headword.is_empty() {
        return invalid_database(path, "representative entry headword is empty".to_owned());
    }

    Ok(CorpusDescriptor {
        corpus_id,
        source_commit,
        source_file_count: actual_files,
        entry_count: actual_entries,
        representative_entry_id,
        representative_headword,
    })
}

fn verify_ready_references(
    connection: &Connection,
    path: &Path,
    corpus_id: &str,
    source_commit: &str,
) -> Result<(), WebDbError> {
    let invalid_canonical_ids: i64 = connection
        .query_row(
            "SELECT count(*) FROM entity \
             WHERE corpus_id = ?1 \
               AND canonical_id NOT LIKE 'kweb:v1/' || ?2 || '/' || dictionary || '/%'",
            params![corpus_id, source_commit],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    if invalid_canonical_ids != 0 {
        return invalid_database(
            path,
            format!("ready corpus has {invalid_canonical_ids} canonical IDs outside its namespace"),
        );
    }

    let invalid_parents: i64 = connection
        .query_row(
            "SELECT count(*) \
             FROM entity AS child \
             JOIN entity AS parent ON parent.canonical_id = child.parent_entry_id \
             WHERE child.corpus_id = ?1 \
               AND (parent.entity_kind <> 'entry' \
                    OR parent.corpus_id <> child.corpus_id \
                    OR parent.relative_path <> child.relative_path)",
            params![corpus_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    if invalid_parents != 0 {
        return invalid_database(
            path,
            format!("ready corpus has {invalid_parents} entities with invalid parent entries"),
        );
    }

    let invalid_projections: i64 = connection
        .query_row(
            "SELECT count(*) \
             FROM entry_projection AS projection \
             JOIN entity ON entity.canonical_id = projection.entry_id \
             WHERE entity.corpus_id = ?1 AND entity.entity_kind <> 'entry'",
            params![corpus_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    if invalid_projections != 0 {
        return invalid_database(
            path,
            format!("ready corpus has {invalid_projections} entry projections with invalid owners"),
        );
    }

    let invalid_text_projections: i64 = connection
        .query_row(
            "SELECT count(*) \
             FROM text_projection AS projection \
             JOIN entity AS entry ON entry.canonical_id = projection.entry_id \
             JOIN entity AS owner ON owner.canonical_id = projection.entity_id \
             WHERE entry.corpus_id = ?1 \
               AND (entry.entity_kind <> 'entry' \
                    OR owner.corpus_id <> entry.corpus_id \
                    OR projection.entry_id <> CASE \
                         WHEN owner.entity_kind = 'entry' THEN owner.canonical_id \
                         ELSE owner.parent_entry_id \
                       END)",
            params![corpus_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    if invalid_text_projections != 0 {
        return invalid_database(
            path,
            format!(
                "ready corpus has {invalid_text_projections} text projections with invalid owners"
            ),
        );
    }

    let invalid_relations: i64 = connection
        .query_row(
            "SELECT count(*) \
             FROM relation \
             JOIN entity AS source ON source.canonical_id = relation.source_entity_id \
             JOIN entity AS source_entry ON source_entry.canonical_id = relation.source_entry_id \
             LEFT JOIN entity AS target ON target.canonical_id = relation.resolved_target_id \
             LEFT JOIN entity AS target_entry ON target_entry.canonical_id = relation.resolved_target_entry_id \
             WHERE source.corpus_id = ?1 \
               AND (source_entry.entity_kind <> 'entry' \
                    OR source_entry.corpus_id <> source.corpus_id \
                    OR relation.source_entry_id <> CASE \
                         WHEN source.entity_kind = 'entry' THEN source.canonical_id \
                         ELSE source.parent_entry_id \
                       END \
                    OR (relation.resolved_target_id IS NOT NULL \
                        AND (target_entry.entity_kind <> 'entry' \
                             OR target.corpus_id <> source.corpus_id \
                             OR target_entry.corpus_id <> source.corpus_id \
                             OR relation.resolved_target_entry_id <> CASE \
                                  WHEN target.entity_kind = 'entry' THEN target.canonical_id \
                                  ELSE target.parent_entry_id \
                                END)))",
            params![corpus_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    if invalid_relations != 0 {
        return invalid_database(
            path,
            format!("ready corpus has {invalid_relations} relations with invalid entity owners"),
        );
    }

    let invalid_candidates: i64 = connection
        .query_row(
            "SELECT count(*) \
             FROM relation_candidate AS candidate \
             JOIN relation ON relation.relation_id = candidate.relation_id \
             JOIN entity AS source ON source.canonical_id = relation.source_entity_id \
             JOIN entity AS target ON target.canonical_id = candidate.candidate_target_id \
             WHERE source.corpus_id = ?1 AND target.corpus_id <> source.corpus_id",
            params![corpus_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    if invalid_candidates != 0 {
        return invalid_database(
            path,
            format!(
                "ready corpus has {invalid_candidates} relation candidates with invalid owners"
            ),
        );
    }

    Ok(())
}

fn count_query(
    connection: &Connection,
    path: &Path,
    sql: &str,
    corpus_id: &str,
) -> Result<u64, WebDbError> {
    let count: i64 = connection
        .query_row(sql, params![corpus_id], |row| row.get(0))
        .map_err(|source| sqlite_error(path, source))?;
    u64::try_from(count).map_err(|_| WebDbError::InvalidDatabase {
        path: path.to_path_buf(),
        reason: format!("negative row count returned by query: {sql}"),
    })
}

fn verify_required_tables(connection: &Connection, path: &Path) -> Result<(), WebDbError> {
    for table in REQUIRED_TABLES {
        let found: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| sqlite_error(path, source))?;
        if found.is_none() {
            return invalid_database(path, format!("required table {table} is missing"));
        }
    }
    Ok(())
}

fn verify_integrity(connection: &Connection, path: &Path) -> Result<(), WebDbError> {
    let mut statement = connection
        .prepare("PRAGMA quick_check")
        .map_err(|source| sqlite_error(path, source))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| sqlite_error(path, source))?;
    let results = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error(path, source))?;
    if results.as_slice() != ["ok"] {
        return invalid_database(path, format!("quick_check failed: {results:?}"));
    }

    let violations: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(|source| sqlite_error(path, source))?;
    if violations != 0 {
        return invalid_database(
            path,
            format!("foreign_key_check found {violations} violations"),
        );
    }
    Ok(())
}

fn schema_fingerprint(connection: &Connection, path: &Path) -> Result<String, WebDbError> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, sql \
             FROM sqlite_schema \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
             ORDER BY type, name",
        )
        .map_err(|source| sqlite_error(path, source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|source| sqlite_error(path, source))?;
    let mut hasher = Sha256::new();
    hasher.update(b"korean-dict-web/schema-fingerprint/v1\0");
    for row in rows {
        let row = row.map_err(|source| sqlite_error(path, source))?;
        for value in [&row.0, &row.1, &row.2, &row.3] {
            let length = u64::try_from(value.len()).expect("SQLite schema strings fit in u64");
            hasher.update(length.to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    Ok(hex_digest(hasher.finalize()))
}

fn expected_schema_fingerprint(path: &Path) -> Result<String, WebDbError> {
    let connection = Connection::open_in_memory().map_err(|source| sqlite_error(path, source))?;
    configure_connection(&connection, path)?;
    connection
        .execute_batch(V1_SQL)
        .map_err(|source| sqlite_error(path, source))?;
    schema_fingerprint(&connection, path)
}

fn migration_checksum(sql: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sql.as_bytes());
    hex_digest(hasher.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn database_has_no_application_schema(
    connection: &Connection,
    path: &Path,
) -> Result<bool, WebDbError> {
    let objects: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_%' AND type IN ('table', 'index', 'view', 'trigger')",
            [],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(path, source))?;
    Ok(objects == 0)
}

fn pragma_i64(connection: &Connection, pragma: &str, path: &Path) -> Result<i64, WebDbError> {
    connection
        .pragma_query_value(None, pragma, |row| row.get(0))
        .map_err(|source| sqlite_error(path, source))
}

fn is_lower_hex_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn resolve_new_path(path: &Path) -> Result<PathBuf, WebDbError> {
    if path.as_os_str().is_empty() {
        return Err(WebDbError::InvalidPath {
            path: path.to_path_buf(),
            reason: "path is empty".to_owned(),
        });
    }
    match fs::symlink_metadata(path) {
        Ok(_) => return Err(WebDbError::ExistingPath(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => Err(WebDbError::Io {
            path: path.to_path_buf(),
            source,
        })?,
    }

    let file_name = path.file_name().ok_or_else(|| WebDbError::InvalidPath {
        path: path.to_path_buf(),
        reason: "path has no file name".to_owned(),
    })?;
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|source| WebDbError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let resolved = parent.join(file_name);
    for suffix in ["-journal", "-wal", "-shm"] {
        let sidecar = path_with_suffix(&resolved, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => return Err(WebDbError::ExistingPath(sidecar)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WebDbError::Io {
                    path: sidecar,
                    source,
                });
            }
        }
    }
    Ok(resolved)
}

fn resolve_existing_path(path: &Path) -> Result<PathBuf, WebDbError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| WebDbError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(WebDbError::InvalidPath {
            path: path.to_path_buf(),
            reason: "path must be a regular file and not a symbolic link".to_owned(),
        });
    }
    fs::canonicalize(path).map_err(|source| WebDbError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn reserve_new_database(path: &Path) -> Result<(), WebDbError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => {
            drop(file);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(WebDbError::ExistingPath(path.to_path_buf()))
        }
        Err(source) => Err(WebDbError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn cleanup_new_database(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        path_with_suffix(path, "-journal"),
        path_with_suffix(path, "-wal"),
        path_with_suffix(path, "-shm"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn sqlite_error(path: &Path, source: rusqlite::Error) -> WebDbError {
    WebDbError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn invalid_database<T>(path: &Path, reason: String) -> Result<T, WebDbError> {
    Err(WebDbError::InvalidDatabase {
        path: path.to_path_buf(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Migration, WebDbError, apply_migration, configure_connection, migrate, pragma_i64,
    };
    use rusqlite::Connection;
    use std::path::Path;

    #[test]
    fn a_failing_migration_rolls_back_all_schema_and_version_changes() {
        let path = Path::new("<memory>");
        let mut connection = Connection::open_in_memory().unwrap();
        configure_connection(&connection, path).unwrap();
        migrate(&mut connection, path).unwrap();

        let error = apply_migration(
            &mut connection,
            path,
            Migration {
                version: 2,
                name: "failing-test",
                sql: "CREATE TABLE should_rollback(value TEXT) STRICT; \
                      INSERT INTO table_that_does_not_exist(value) VALUES ('fail');",
            },
        )
        .unwrap_err();
        assert!(matches!(error, WebDbError::Sqlite { .. }));
        assert_eq!(pragma_i64(&connection, "user_version", path).unwrap(), 1);
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'should_rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }
}
