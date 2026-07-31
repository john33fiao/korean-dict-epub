use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use atomic_write_file::AtomicWriteFile;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::web_db::{
    DatabaseDescriptor, ValidationLevel, WebDbError, create_new, finalize_import,
    initialize_importing_corpus, open_existing_for_import, validate, validate_importing_corpus,
    validate_source_file_completion,
};
pub use crate::web_db::{ExpectedCorpus, SourceFileCompletion, SourceFileIdentity};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    New,
    Resume,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransactionOutcome {
    Committed(SourceFileCompletion),
    AlreadyComplete(SourceFileCompletion),
}

#[derive(Debug)]
pub enum ImportError {
    WebDb(WebDbError),
    InvalidLifecycle { path: PathBuf, reason: String },
    Io { path: PathBuf, source: io::Error },
    Operation(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebDb(error) => write!(formatter, "{error}"),
            Self::InvalidLifecycle { path, reason } => {
                write!(
                    formatter,
                    "invalid import lifecycle for '{}': {reason}",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "import I/O error for '{}': {source}",
                    path.display()
                )
            }
            Self::Operation(reason) => write!(formatter, "source file import failed: {reason}"),
        }
    }
}

impl Error for ImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WebDb(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::InvalidLifecycle { .. } | Self::Operation(_) => None,
        }
    }
}

impl From<WebDbError> for ImportError {
    fn from(value: WebDbError) -> Self {
        Self::WebDb(value)
    }
}

pub struct ImportSession {
    mode: ImportMode,
    destination_path: PathBuf,
    working_path: PathBuf,
    source_commit: String,
    connection: Option<Connection>,
    cleanup_working_on_drop: bool,
}

impl fmt::Debug for ImportSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportSession")
            .field("mode", &self.mode)
            .field("destination_path", &self.destination_path)
            .field("working_path", &self.working_path)
            .field("source_commit", &self.source_commit)
            .finish_non_exhaustive()
    }
}

pub fn begin_import(
    destination: &Path,
    source_commit: &str,
    mode: ImportMode,
) -> Result<ImportSession, ImportError> {
    if !is_lower_hex(source_commit, 40) {
        return lifecycle_error(
            destination,
            "source commit must be 40 lowercase hexadecimal characters",
        );
    }

    match mode {
        ImportMode::New => begin_new(destination, source_commit, mode, false),
        ImportMode::Resume => {
            let (working_path, connection) = open_existing_for_import(destination)?;
            validate_importing_corpus(&connection, &working_path, source_commit, true)?;
            Ok(ImportSession {
                mode,
                destination_path: working_path.clone(),
                working_path,
                source_commit: source_commit.to_owned(),
                connection: Some(connection),
                cleanup_working_on_drop: false,
            })
        }
        ImportMode::Rebuild => {
            let destination_path = resolve_rebuild_destination(destination)?;
            reject_sidecars(&destination_path)?;
            let working_path = unique_staging_path(&destination_path)?;
            begin_new_at_paths(destination_path, working_path, source_commit, mode, true)
        }
    }
}

impl ImportSession {
    pub fn mode(&self) -> ImportMode {
        self.mode
    }

    pub fn destination_path(&self) -> &Path {
        &self.destination_path
    }

    pub fn working_path(&self) -> &Path {
        &self.working_path
    }

    pub fn with_file_transaction<F>(
        &mut self,
        identity: &SourceFileIdentity,
        operation: F,
    ) -> Result<FileTransactionOutcome, ImportError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<SourceFileCompletion, ImportError>,
    {
        let path = self.working_path.clone();
        let relative_path = normalized_relative_path(&identity.relative_path, &path)?;
        validate_file_identity(identity, &path)?;
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| ImportError::InvalidLifecycle {
                path: path.clone(),
                reason: "import session is already finalized".to_owned(),
            })?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&path, source))?;
        validate_importing_corpus(&transaction, &path, &self.source_commit, false)?;

        if let Some(existing) =
            read_existing_file(&transaction, &path, &self.source_commit, &relative_path)?
        {
            if !existing.matches_identity(identity)? {
                return lifecycle_error(
                    &path,
                    "completed source file identity does not match the catalog",
                );
            }
            let completion = existing
                .completion
                .ok_or_else(|| ImportError::InvalidLifecycle {
                    path: path.clone(),
                    reason: format!("source file {relative_path} is only partially committed"),
                })?;
            validate_file_row_counts(
                &transaction,
                &path,
                &self.source_commit,
                &relative_path,
                &completion,
            )?;
            return Ok(FileTransactionOutcome::AlreadyComplete(completion));
        }

        transaction
            .execute(
                "INSERT INTO source_file( \
                     corpus_id, relative_path, dictionary, source_ordinal, volume_number, volume_total \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    self.source_commit,
                    relative_path,
                    identity.dictionary.key(),
                    to_i64(identity.source_ordinal, &path, "source ordinal")?,
                    to_i64(identity.volume_number, &path, "volume number")?,
                    to_i64(identity.volume_total, &path, "volume total")?,
                ],
            )
            .map_err(|source| sqlite_error(&path, source))?;

        let completion = operation(&transaction)?;
        validate_completion(&completion, &path)?;
        transaction
            .execute(
                "UPDATE source_file SET record_sha256 = ?1, record_count = ?2, entry_count = ?3 \
                 WHERE corpus_id = ?4 AND relative_path = ?5",
                params![
                    completion.record_sha256,
                    to_i64(completion.record_count, &path, "record count")?,
                    to_i64(completion.entry_count, &path, "entry count")?,
                    self.source_commit,
                    relative_path,
                ],
            )
            .map_err(|source| sqlite_error(&path, source))?;
        validate_file_row_counts(
            &transaction,
            &path,
            &self.source_commit,
            &relative_path,
            &completion,
        )?;
        let violations: i64 = transaction
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .map_err(|source| sqlite_error(&path, source))?;
        if violations != 0 {
            return lifecycle_error(
                &path,
                &format!("foreign_key_check found {violations} violations"),
            );
        }
        transaction
            .commit()
            .map_err(|source| sqlite_error(&path, source))?;
        Ok(FileTransactionOutcome::Committed(completion))
    }

    pub fn finalize(
        mut self,
        expected: &ExpectedCorpus,
    ) -> Result<DatabaseDescriptor, ImportError> {
        if expected.source_commit != self.source_commit {
            return lifecycle_error(
                &self.working_path,
                "expected corpus source commit differs from the import session",
            );
        }
        let mut connection =
            self.connection
                .take()
                .ok_or_else(|| ImportError::InvalidLifecycle {
                    path: self.working_path.clone(),
                    reason: "import session is already finalized".to_owned(),
                })?;
        finalize_import(&mut connection, &self.working_path, expected)?;
        drop(connection);
        validate(&self.working_path, ValidationLevel::ReadyCorpus)?;

        if self.mode == ImportMode::Rebuild {
            reject_sidecars(&self.destination_path)?;
            publish_database(&self.working_path, &self.destination_path)?;
            cleanup_database_files(&self.working_path);
            self.cleanup_working_on_drop = false;
        }
        validate(&self.destination_path, ValidationLevel::ReadyCorpus).map_err(ImportError::from)
    }
}

impl Drop for ImportSession {
    fn drop(&mut self) {
        if self.cleanup_working_on_drop {
            self.connection.take();
            cleanup_database_files(&self.working_path);
        }
    }
}

fn begin_new(
    destination: &Path,
    source_commit: &str,
    mode: ImportMode,
    cleanup_working_on_drop: bool,
) -> Result<ImportSession, ImportError> {
    let destination_path = resolve_absent_destination(destination)?;
    begin_new_at_paths(
        destination_path.clone(),
        destination_path,
        source_commit,
        mode,
        cleanup_working_on_drop,
    )
}

fn begin_new_at_paths(
    destination_path: PathBuf,
    working_path: PathBuf,
    source_commit: &str,
    mode: ImportMode,
    cleanup_working_on_drop: bool,
) -> Result<ImportSession, ImportError> {
    let mut connection = create_new(&working_path)?;
    if let Err(error) = initialize_importing_corpus(&mut connection, &working_path, source_commit) {
        drop(connection);
        cleanup_database_files(&working_path);
        return Err(error.into());
    }
    Ok(ImportSession {
        mode,
        destination_path,
        working_path,
        source_commit: source_commit.to_owned(),
        connection: Some(connection),
        cleanup_working_on_drop,
    })
}

struct ExistingSourceFile {
    dictionary: String,
    source_ordinal: i64,
    volume_number: i64,
    volume_total: i64,
    completion: Option<SourceFileCompletion>,
}

impl ExistingSourceFile {
    fn matches_identity(&self, expected: &SourceFileIdentity) -> Result<bool, ImportError> {
        Ok(self.dictionary == expected.dictionary.key()
            && self.source_ordinal == i64::try_from(expected.source_ordinal).unwrap_or(i64::MIN)
            && self.volume_number == i64::try_from(expected.volume_number).unwrap_or(i64::MIN)
            && self.volume_total == i64::try_from(expected.volume_total).unwrap_or(i64::MIN))
    }
}

fn read_existing_file(
    connection: &Connection,
    path: &Path,
    source_commit: &str,
    relative_path: &str,
) -> Result<Option<ExistingSourceFile>, ImportError> {
    connection
        .query_row(
            "SELECT dictionary, source_ordinal, volume_number, volume_total, \
                    record_sha256, record_count, entry_count \
             FROM source_file WHERE corpus_id = ?1 AND relative_path = ?2",
            params![source_commit, relative_path],
            |row| {
                let digest: Option<String> = row.get(4)?;
                let records: Option<i64> = row.get(5)?;
                let entries: Option<i64> = row.get(6)?;
                let completion = match (digest, records, entries) {
                    (Some(record_sha256), Some(record_count), Some(entry_count)) => {
                        match (u64::try_from(record_count), u64::try_from(entry_count)) {
                            (Ok(record_count), Ok(entry_count)) => Some(SourceFileCompletion {
                                record_sha256,
                                record_count,
                                entry_count,
                            }),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                Ok(ExistingSourceFile {
                    dictionary: row.get(0)?,
                    source_ordinal: row.get(1)?,
                    volume_number: row.get(2)?,
                    volume_total: row.get(3)?,
                    completion,
                })
            },
        )
        .optional()
        .map_err(|source| sqlite_error(path, source))
}

fn validate_file_row_counts(
    connection: &Connection,
    path: &Path,
    source_commit: &str,
    relative_path: &str,
    completion: &SourceFileCompletion,
) -> Result<(), ImportError> {
    validate_completion(completion, path)?;
    validate_source_file_completion(connection, path, source_commit, relative_path, completion)
        .map_err(ImportError::from)
}

fn validate_file_identity(identity: &SourceFileIdentity, path: &Path) -> Result<(), ImportError> {
    if identity.volume_number == 0
        || identity.volume_total < identity.volume_number
        || identity.source_ordinal > i64::MAX as u64
        || identity.volume_number > i64::MAX as u64
        || identity.volume_total > i64::MAX as u64
    {
        return lifecycle_error(
            path,
            "source file identity contains invalid ordinal or volume values",
        );
    }
    Ok(())
}

fn validate_completion(completion: &SourceFileCompletion, path: &Path) -> Result<(), ImportError> {
    if !is_lower_hex(&completion.record_sha256, 64)
        || completion.record_count > i64::MAX as u64
        || completion.entry_count > i64::MAX as u64
    {
        return lifecycle_error(
            path,
            "source file completion contains invalid digest or counts",
        );
    }
    Ok(())
}

fn normalized_relative_path(path: &Path, database_path: &Path) -> Result<String, ImportError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return lifecycle_error(
            database_path,
            "source file path must be a safe relative path",
        );
    }
    let value = path.to_str().ok_or_else(|| ImportError::InvalidLifecycle {
        path: database_path.to_path_buf(),
        reason: "source file path must be valid UTF-8".to_owned(),
    })?;
    Ok(value.replace('\\', "/"))
}

fn resolve_absent_destination(path: &Path) -> Result<PathBuf, ImportError> {
    let resolved = resolve_parent_and_name(path)?;
    match fs::symlink_metadata(&resolved) {
        Ok(_) => return lifecycle_error(&resolved, "new import destination already exists"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&resolved, source)),
    }
    reject_sidecars(&resolved)?;
    Ok(resolved)
}

fn resolve_rebuild_destination(path: &Path) -> Result<PathBuf, ImportError> {
    let resolved = resolve_parent_and_name(path)?;
    match fs::symlink_metadata(&resolved) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return lifecycle_error(
                &resolved,
                "rebuild destination must be a regular file or absent",
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&resolved, source)),
    }
    Ok(resolved)
}

fn resolve_parent_and_name(path: &Path) -> Result<PathBuf, ImportError> {
    if path.as_os_str().is_empty() {
        return lifecycle_error(path, "database path is empty");
    }
    let name = path
        .file_name()
        .ok_or_else(|| ImportError::InvalidLifecycle {
            path: path.to_path_buf(),
            reason: "database path has no file name".to_owned(),
        })?;
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|source| io_error(parent, source))?;
    Ok(parent.join(name))
}

fn unique_staging_path(destination: &Path) -> Result<PathBuf, ImportError> {
    let name = destination
        .file_name()
        .expect("resolved destination has a file name");
    for _ in 0..1024 {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut staging_name = OsString::from(".");
        staging_name.push(name);
        staging_name.push(format!(
            ".kweb-rebuild-{}-{sequence}.sqlite",
            std::process::id()
        ));
        let staging = destination.with_file_name(staging_name);
        if !staging.exists()
            && !sidecar_paths(&staging)
                .iter()
                .any(|candidate| candidate.exists())
        {
            return Ok(staging);
        }
    }
    lifecycle_error(
        destination,
        "could not reserve a unique rebuild staging path",
    )
}

fn reject_sidecars(path: &Path) -> Result<(), ImportError> {
    for sidecar in sidecar_paths(path) {
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => return lifecycle_error(&sidecar, "SQLite sidecar exists"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&sidecar, source)),
        }
    }
    Ok(())
}

fn sidecar_paths(path: &Path) -> [PathBuf; 3] {
    ["-journal", "-wal", "-shm"].map(|suffix| append_suffix(path, suffix))
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn publish_database(staging: &Path, destination: &Path) -> Result<(), ImportError> {
    publish_database_with(staging, destination, |source, output| {
        io::copy(source, output)
    })
}

fn publish_database_with<F>(staging: &Path, destination: &Path, copy: F) -> Result<(), ImportError>
where
    F: FnOnce(&mut File, &mut AtomicWriteFile) -> io::Result<u64>,
{
    let mut source = File::open(staging).map_err(|error| io_error(staging, error))?;
    let mut output =
        AtomicWriteFile::open(destination).map_err(|error| io_error(destination, error))?;
    copy(&mut source, &mut output).map_err(|error| io_error(destination, error))?;
    output
        .as_file()
        .sync_all()
        .map_err(|error| io_error(destination, error))?;
    output
        .commit()
        .map_err(|error| io_error(destination, error))
}

fn cleanup_database_files(path: &Path) {
    for candidate in std::iter::once(path.to_path_buf()).chain(sidecar_paths(path)) {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn to_i64(value: u64, path: &Path, label: &str) -> Result<i64, ImportError> {
    i64::try_from(value).map_err(|_| ImportError::InvalidLifecycle {
        path: path.to_path_buf(),
        reason: format!("{label} exceeds SQLite INTEGER range"),
    })
}

fn sqlite_error(path: &Path, source: rusqlite::Error) -> ImportError {
    ImportError::WebDb(WebDbError::Sqlite {
        path: path.to_path_buf(),
        source,
    })
}

fn io_error(path: &Path, source: io::Error) -> ImportError {
    ImportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn lifecycle_error<T>(path: &Path, reason: &str) -> Result<T, ImportError> {
    Err(ImportError::InvalidLifecycle {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::publish_database_with;
    use std::fs;
    use std::io::{self, Write};

    #[test]
    fn simulated_low_disk_failure_discards_the_atomic_replacement() {
        let root =
            std::env::temp_dir().join(format!("kweb-import-low-disk-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        let staging = root.join("staging.sqlite");
        let destination = root.join("dictionary.sqlite");
        fs::write(&staging, b"validated-new-database").unwrap();
        fs::write(&destination, b"old-database").unwrap();

        let result = publish_database_with(&staging, &destination, |_, output| {
            output.write_all(b"partial-new")?;
            Err(io::Error::other("simulated no space left on device"))
        });

        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"old-database");
        fs::remove_dir_all(root).unwrap();
    }
}
