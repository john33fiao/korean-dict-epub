use std::collections::HashSet;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use atomic_write_file::AtomicWriteFile;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::record::{CanonicalDigest, SourceAttribute, SourceRecord};
use crate::source::{SourceError, SourceRecordReader};
use crate::web_db::{
    DatabaseDescriptor, ValidationLevel, WebDbError, create_new, finalize_import,
    initialize_importing_corpus, open_existing_for_import, validate, validate_importing_corpus,
    validate_source_file_completion,
};
pub use crate::web_db::{ExpectedCorpus, SourceFileCompletion, SourceFileIdentity};
use crate::web_identity::{CanonicalIdError, CanonicalIdParts, EntityKind, canonical_id};

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
    Source { path: PathBuf, source: SourceError },
    CanonicalId(CanonicalIdError),
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
            Self::Source { path, source } => {
                write!(
                    formatter,
                    "source XML error for '{}': {source}",
                    path.display()
                )
            }
            Self::CanonicalId(error) => write!(formatter, "canonical ID error: {error}"),
            Self::Operation(reason) => write!(formatter, "source file import failed: {reason}"),
        }
    }
}

impl Error for ImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WebDb(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Source { source, .. } => Some(source),
            Self::CanonicalId(error) => Some(error),
            Self::InvalidLifecycle { .. } | Self::Operation(_) => None,
        }
    }
}

impl From<WebDbError> for ImportError {
    fn from(value: WebDbError) -> Self {
        Self::WebDb(value)
    }
}

impl From<CanonicalIdError> for ImportError {
    fn from(value: CanonicalIdError) -> Self {
        Self::CanonicalId(value)
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

    pub fn import_source_file(
        &mut self,
        identity: &SourceFileIdentity,
        source_path: &Path,
    ) -> Result<FileTransactionOutcome, ImportError> {
        let metadata =
            fs::symlink_metadata(source_path).map_err(|source| io_error(source_path, source))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return lifecycle_error(source_path, "source XML must be a regular non-symlink file");
        }
        let input = File::open(source_path).map_err(|source| io_error(source_path, source))?;
        let source_commit = self.source_commit.clone();
        let source_path = source_path.to_path_buf();
        let relative_path = identity.relative_path.to_string_lossy().replace('\\', "/");
        self.with_file_transaction(identity, |transaction| {
            import_source_reader(
                transaction,
                &source_path,
                &source_commit,
                identity,
                &relative_path,
                input,
            )
        })
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

struct EntryImportState {
    root_depth: usize,
    root_name: String,
    start_record_ordinal: u64,
    entry_ordinal: u64,
    path: Vec<String>,
    native_key: Option<String>,
    headword: Option<String>,
    homonym_number: Option<String>,
    headword_record_ordinal: Option<u64>,
}

impl EntryImportState {
    fn new(
        identity: &SourceFileIdentity,
        depth: usize,
        name: &str,
        attributes: &[SourceAttribute],
        record_ordinal: u64,
        entry_ordinal: u64,
    ) -> Self {
        let native_key = match identity.dictionary {
            crate::catalog::Dictionary::Krdict => lmf_identifier(attributes),
            crate::catalog::Dictionary::Stdict | crate::catalog::Dictionary::Opendict => {
                attribute(attributes, "target_code")
            }
        };
        Self {
            root_depth: depth,
            root_name: local_name(name).to_owned(),
            start_record_ordinal: record_ordinal,
            entry_ordinal,
            path: vec![local_name(name).to_owned()],
            native_key,
            headword: None,
            homonym_number: None,
            headword_record_ordinal: None,
        }
    }

    fn start(&mut self, name: &str) {
        self.path.push(local_name(name).to_owned());
    }

    fn empty(
        &mut self,
        dictionary: crate::catalog::Dictionary,
        name: &str,
        attributes: &[SourceAttribute],
        record_ordinal: u64,
    ) {
        if dictionary != crate::catalog::Dictionary::Krdict || local_name(name) != "feat" {
            return;
        }
        let Some(field) = attribute(attributes, "att") else {
            return;
        };
        let value = attribute(attributes, "val").unwrap_or_default();
        if field == "writtenForm" && self.path.iter().any(|name| name == "Lemma") {
            set_once(&mut self.headword, value);
            if self.headword_record_ordinal.is_none() {
                self.headword_record_ordinal = Some(record_ordinal);
            }
        } else if field == "homonym_number" && self.path.len() == 1 {
            set_once(&mut self.homonym_number, value);
        }
    }

    fn text(&mut self, dictionary: crate::catalog::Dictionary, value: &str, record_ordinal: u64) {
        let Some(name) = self.path.last().map(String::as_str) else {
            return;
        };
        match (dictionary, name) {
            (
                crate::catalog::Dictionary::Stdict | crate::catalog::Dictionary::Opendict,
                "target_code",
            ) if self.path.len() == 2 => set_once(&mut self.native_key, value.to_owned()),
            (crate::catalog::Dictionary::Stdict, "word")
                if self.path.iter().any(|name| name == "word_info") =>
            {
                set_once(&mut self.headword, value.to_owned());
                if self.headword_record_ordinal.is_none() {
                    self.headword_record_ordinal = Some(record_ordinal);
                }
            }
            (crate::catalog::Dictionary::Opendict, "word")
                if self.path.iter().any(|name| name == "wordInfo") =>
            {
                set_once(&mut self.headword, value.to_owned());
                if self.headword_record_ordinal.is_none() {
                    self.headword_record_ordinal = Some(record_ordinal);
                }
            }
            (crate::catalog::Dictionary::Opendict, "group_order") if self.path.len() == 2 => {
                set_once(&mut self.homonym_number, value.to_owned());
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &str) -> Result<(), ImportError> {
        let actual = local_name(name);
        let expected = self.path.pop().ok_or_else(|| {
            ImportError::Operation("entry element stack ended unexpectedly".to_owned())
        })?;
        if actual != expected {
            return Err(ImportError::Operation(format!(
                "entry element stack differs: ended {actual}, expected {expected}"
            )));
        }
        Ok(())
    }
}

fn import_source_reader<R: Read>(
    transaction: &Transaction<'_>,
    source_path: &Path,
    source_commit: &str,
    identity: &SourceFileIdentity,
    relative_path: &str,
    input: R,
) -> Result<SourceFileCompletion, ImportError> {
    let mut digest = CanonicalDigest::new();
    let mut record_count = 0_u64;
    let mut entry_count = 0_u64;
    let mut entry = None;
    let mut native_keys = HashSet::new();

    for record in SourceRecordReader::new(input) {
        let record = record.map_err(|source| ImportError::Source {
            path: source_path.to_path_buf(),
            source,
        })?;
        let record_ordinal = record_count;
        insert_source_record(
            transaction,
            source_path,
            source_commit,
            relative_path,
            record_ordinal,
            &record,
        )?;
        digest.update(&record);
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| ImportError::Operation("source record count overflow".to_owned()))?;

        match &record {
            SourceRecord::StartElement {
                depth,
                name,
                attributes,
            } => {
                if identity.dictionary.is_entry_element(name) {
                    if entry.is_some() {
                        return Err(ImportError::Operation(
                            "nested dictionary entries are not supported".to_owned(),
                        ));
                    }
                    entry_count = entry_count.checked_add(1).ok_or_else(|| {
                        ImportError::Operation("source entry count overflow".to_owned())
                    })?;
                    entry = Some(EntryImportState::new(
                        identity,
                        *depth,
                        name,
                        attributes,
                        record_ordinal,
                        entry_count,
                    ));
                } else if let Some(entry) = entry.as_mut() {
                    entry.start(name);
                }
            }
            SourceRecord::EmptyElement {
                name, attributes, ..
            } => {
                if identity.dictionary.is_entry_element(name) {
                    return Err(ImportError::Operation(
                        "empty dictionary entries are not supported".to_owned(),
                    ));
                }
                if let Some(entry) = entry.as_mut() {
                    entry.empty(identity.dictionary, name, attributes, record_ordinal);
                }
            }
            SourceRecord::ElementText { value, .. } => {
                if let Some(entry) = entry.as_mut() {
                    entry.text(identity.dictionary, value, record_ordinal);
                }
            }
            SourceRecord::TailText { .. } => {}
            SourceRecord::EndElement { depth, name } => {
                let finishes_entry = entry.as_ref().is_some_and(|entry| {
                    *depth == entry.root_depth && local_name(name) == entry.root_name
                });
                if finishes_entry {
                    let completed = entry.take().expect("entry state exists");
                    finish_entry(
                        transaction,
                        source_path,
                        source_commit,
                        identity,
                        relative_path,
                        record_ordinal,
                        completed,
                        &mut native_keys,
                    )?;
                } else if let Some(entry) = entry.as_mut() {
                    entry.end(name)?;
                }
            }
        }
    }

    if entry.is_some() {
        return Err(ImportError::Operation(
            "source ended inside a dictionary entry".to_owned(),
        ));
    }
    let summary = digest.finalize();
    Ok(SourceFileCompletion {
        record_sha256: summary.sha256,
        record_count,
        entry_count,
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_entry(
    transaction: &Transaction<'_>,
    source_path: &Path,
    source_commit: &str,
    identity: &SourceFileIdentity,
    relative_path: &str,
    end_record_ordinal: u64,
    entry: EntryImportState,
    native_keys: &mut HashSet<String>,
) -> Result<(), ImportError> {
    let native_key = normalized_required(entry.native_key, "native key")?;
    let headword = normalized_required(entry.headword, "headword")?;
    if !native_keys.insert(native_key.clone()) {
        return Err(ImportError::Operation(format!(
            "duplicate native key {native_key:?} in one source file"
        )));
    }
    let existing: i64 = transaction
        .query_row(
            "SELECT count(*) FROM entity \
             WHERE corpus_id = ?1 AND dictionary = ?2 AND entity_kind = 'entry' AND native_key = ?3",
            params![source_commit, identity.dictionary.key(), native_key],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error(source_path, source))?;
    if existing != 0 {
        return Err(ImportError::Operation(format!(
            "duplicate native key {native_key:?} in the fixture corpus"
        )));
    }

    let source_locator = format!(
        "{}:{}#entry={}",
        identity.dictionary.key(),
        relative_path,
        entry.entry_ordinal
    );
    let entry_id = canonical_id(CanonicalIdParts {
        corpus_commit: source_commit,
        dictionary: identity.dictionary,
        entity_kind: EntityKind::Entry,
        native_key: Some(&native_key),
        owning_entry_id: None,
        source_locator: &source_locator,
        namespace_occurrences: 1,
    })?;
    transaction
        .execute(
            "INSERT INTO entity( \
                 canonical_id, corpus_id, relative_path, dictionary, entity_kind, native_key, \
                 parent_entry_id, source_locator, entry_ordinal, kind_ordinal \
             ) VALUES (?1, ?2, ?3, ?4, 'entry', ?5, NULL, ?6, ?7, 0)",
            params![
                entry_id,
                source_commit,
                relative_path,
                identity.dictionary.key(),
                native_key,
                source_locator,
                to_i64(entry.entry_ordinal, source_path, "entry ordinal")?,
            ],
        )
        .map_err(|source| sqlite_error(source_path, source))?;
    transaction
        .execute(
            "UPDATE source_record SET entry_id = ?1 \
             WHERE corpus_id = ?2 AND relative_path = ?3 \
               AND record_ordinal BETWEEN ?4 AND ?5",
            params![
                entry_id,
                source_commit,
                relative_path,
                to_i64(
                    entry.start_record_ordinal,
                    source_path,
                    "entry start record ordinal"
                )?,
                to_i64(end_record_ordinal, source_path, "entry end record ordinal")?,
            ],
        )
        .map_err(|source| sqlite_error(source_path, source))?;
    transaction
        .execute(
            "INSERT INTO entry_projection( \
                 entry_id, headword, homonym_number, headword_record_ordinal \
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                entry_id,
                headword,
                normalized_optional(entry.homonym_number),
                entry
                    .headword_record_ordinal
                    .map(|value| to_i64(value, source_path, "headword record ordinal"))
                    .transpose()?,
            ],
        )
        .map_err(|source| sqlite_error(source_path, source))?;
    Ok(())
}

fn insert_source_record(
    transaction: &Transaction<'_>,
    source_path: &Path,
    source_commit: &str,
    relative_path: &str,
    record_ordinal: u64,
    record: &SourceRecord,
) -> Result<(), ImportError> {
    let (kind, depth, qname, text_value, attributes) = match record {
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
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7)",
            params![
                source_commit,
                relative_path,
                to_i64(record_ordinal, source_path, "record ordinal")?,
                kind,
                to_i64(
                    u64::try_from(depth).map_err(|_| ImportError::Operation(
                        "record depth does not fit in u64".to_owned()
                    ))?,
                    source_path,
                    "record depth"
                )?,
                qname,
                text_value,
            ],
        )
        .map_err(|source| sqlite_error(source_path, source))?;
    for (attribute_ordinal, attribute) in attributes.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO source_attribute( \
                     corpus_id, relative_path, record_ordinal, attribute_ordinal, qname, attribute_value \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    source_commit,
                    relative_path,
                    to_i64(record_ordinal, source_path, "record ordinal")?,
                    to_i64(
                        u64::try_from(attribute_ordinal).map_err(|_| {
                            ImportError::Operation(
                                "attribute ordinal does not fit in u64".to_owned(),
                            )
                        })?,
                        source_path,
                        "attribute ordinal"
                    )?,
                    attribute.name,
                    attribute.value,
                ],
            )
            .map_err(|source| sqlite_error(source_path, source))?;
    }
    Ok(())
}

fn attribute(attributes: &[SourceAttribute], name: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| local_name(&attribute.name) == name)
        .map(|attribute| attribute.value.clone())
}

fn lmf_identifier(attributes: &[SourceAttribute]) -> Option<String> {
    if attribute(attributes, "att").as_deref() == Some("id") {
        return attribute(attributes, "val");
    }
    attribute(attributes, "id")
}

fn local_name(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, local)| local)
}

fn set_once(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

fn normalized_required(value: Option<String>, label: &str) -> Result<String, ImportError> {
    normalized_optional(value)
        .ok_or_else(|| ImportError::Operation(format!("dictionary entry has no {label}")))
}

fn normalized_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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
    use super::{
        ExpectedCorpus, ImportMode, SourceFileIdentity, begin_import, import_source_reader,
        publish_database_with,
    };
    use crate::catalog::Dictionary;
    use crate::record::CanonicalDigest;
    use crate::source::SourceRecordReader;
    use rusqlite::{Connection, params};
    use std::fs;
    use std::io::{self, Cursor, Read, Write};
    use std::path::{Path, PathBuf};

    const COMMIT: &str = "42c0d01889f34536e9cf94fe57f62bd2055b1bde";
    const XML: &str = r#"<LexicalResource><Lexicon><LexicalEntry att="id" val="0001"><feat att="homonym_number" val="1"/><Lemma><feat att="writtenForm" val="표제어"/></Lemma><future:opaque xmlns:future="urn:test" zeta="첫째" alpha="둘째">보존</future:opaque></LexicalEntry></Lexicon></LexicalResource>"#;

    struct OneByteReader<R>(R);

    impl<R: Read> Read for OneByteReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(buffer.len() <= 8 * 1024);
            let limit = buffer.len().min(1);
            self.0.read(&mut buffer[..limit])
        }
    }

    struct FailingReader<R> {
        inner: R,
        remaining: usize,
    }

    impl<R: Read> Read for FailingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("injected streaming read failure"));
            }
            let limit = buffer.len().min(self.remaining).min(16);
            let read = self.inner.read(&mut buffer[..limit])?;
            self.remaining -= read;
            Ok(read)
        }
    }

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

    #[test]
    fn one_byte_chunks_produce_the_same_ready_digest() {
        let root = temp_root("one-byte");
        let database = root.join("dictionary.sqlite");
        let identity = source_identity(0, "krdict/fixture.xml");
        let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
        let relative_path = "krdict/fixture.xml";
        session
            .with_file_transaction(&identity, |transaction| {
                import_source_reader(
                    transaction,
                    Path::new("one-byte.xml"),
                    COMMIT,
                    &identity,
                    relative_path,
                    OneByteReader(Cursor::new(XML.as_bytes())),
                )
            })
            .unwrap();
        session
            .finalize(&ExpectedCorpus {
                source_commit: COMMIT.to_owned(),
                source_files: vec![identity],
            })
            .unwrap();

        let mut digest = CanonicalDigest::new();
        let mut count = 0_u64;
        for record in SourceRecordReader::new(Cursor::new(XML.as_bytes())) {
            digest.update(&record.unwrap());
            count += 1;
        }
        let expected = digest.finalize();
        let connection = Connection::open(&database).unwrap();
        let actual: (String, i64) = connection
            .query_row(
                "SELECT record_sha256, record_count FROM source_file",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(actual, (expected.sha256, i64::try_from(count).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn field_omission_and_attribute_reordering_roll_back_the_file() {
        for (label, mutation) in [
            (
                "omitted-field",
                "DELETE FROM source_attribute WHERE qname = 'alpha'",
            ),
            (
                "reordered-attributes",
                "UPDATE source_attribute \
                 SET attribute_ordinal = CASE qname WHEN 'zeta' THEN 101 WHEN 'alpha' THEN 100 ELSE attribute_ordinal END \
                 WHERE qname IN ('zeta', 'alpha')",
            ),
        ] {
            let root = temp_root(label);
            let database = root.join("dictionary.sqlite");
            let identity = source_identity(0, "krdict/fixture.xml");
            let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
            let result = session.with_file_transaction(&identity, |transaction| {
                let completion = import_source_reader(
                    transaction,
                    Path::new("corrupted.xml"),
                    COMMIT,
                    &identity,
                    "krdict/fixture.xml",
                    Cursor::new(XML.as_bytes()),
                )?;
                transaction.execute_batch(mutation).unwrap();
                Ok(completion)
            });
            assert!(result.is_err(), "{label} must fail completion validation");
            drop(session);
            assert_empty_payload(&database);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn streaming_io_error_rolls_back_only_the_current_file() {
        let root = temp_root("stream-error");
        let database = root.join("dictionary.sqlite");
        let first = source_identity(0, "krdict/first.xml");
        let second = source_identity(1, "krdict/second.xml");
        let mut session = begin_import(&database, COMMIT, ImportMode::New).unwrap();
        session
            .with_file_transaction(&first, |transaction| {
                import_source_reader(
                    transaction,
                    Path::new("first.xml"),
                    COMMIT,
                    &first,
                    "krdict/first.xml",
                    Cursor::new(XML.as_bytes()),
                )
            })
            .unwrap();
        let result = session.with_file_transaction(&second, |transaction| {
            import_source_reader(
                transaction,
                Path::new("second.xml"),
                COMMIT,
                &second,
                "krdict/second.xml",
                FailingReader {
                    inner: Cursor::new(XML.as_bytes()),
                    remaining: 96,
                },
            )
        });
        assert!(result.is_err());
        drop(session);

        let connection = Connection::open(&database).unwrap();
        let files: Vec<String> = connection
            .prepare("SELECT relative_path FROM source_file ORDER BY source_ordinal")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(files, ["krdict/first.xml"]);
        let second_rows: i64 = connection
            .query_row(
                "SELECT count(*) FROM source_record WHERE relative_path = ?1",
                params!["krdict/second.xml"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second_rows, 0);
        drop(connection);
        fs::remove_dir_all(root).unwrap();
    }

    fn source_identity(source_ordinal: u64, relative_path: &str) -> SourceFileIdentity {
        SourceFileIdentity {
            relative_path: PathBuf::from(relative_path),
            dictionary: Dictionary::Krdict,
            source_ordinal,
            volume_number: source_ordinal + 1,
            volume_total: 2.max(source_ordinal + 1),
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("kweb-import-{label}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn assert_empty_payload(database: &Path) {
        let connection = Connection::open(database).unwrap();
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
        assert_eq!(counts, (0, 0, 0));
    }
}
