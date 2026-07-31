use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use atomic_write_file::AtomicWriteFile;
use serde::Serialize;

use crate::catalog::{self, Dictionary, Volume};
use crate::record::{SourceAttribute, SourceRecord};
use crate::source::{SourceError, SourceRecordReader};

pub const REPORT_SCHEMA: &str = "kweb-source-identifiers-relations-v1";
pub const DEFAULT_SOURCE: &str = "references/korean-dict-nikl";
pub const DEFAULT_OUTPUT: &str = "outputs/kweb-002";

const JSON_FILENAME: &str = "source-identifiers-relations-v1.json";
const MARKDOWN_FILENAME: &str = "source-identifiers-relations-v1.md";
const DUPLICATE_SAMPLE_LIMIT: usize = 5;
const COLLISION_SAMPLE_LIMIT: usize = 10;
const CYCLE_SAMPLE_LIMIT: usize = 10;

#[derive(Debug)]
pub enum WebSourceAuditError {
    Catalog(catalog::CatalogError),
    Source {
        path: PathBuf,
        source: SourceError,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Git {
        path: PathBuf,
        message: String,
    },
    DirtySource(PathBuf),
    InvalidOutput {
        path: PathBuf,
        reason: String,
    },
    UnsafeOutput {
        source: PathBuf,
        output: PathBuf,
    },
    ExistingOutput(PathBuf),
    Serialization(serde_json::Error),
    InvalidSourceShape {
        path: PathBuf,
        reason: String,
    },
}

impl fmt::Display for WebSourceAuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "source catalog error: {error}"),
            Self::Source { path, source } => {
                write!(formatter, "could not parse '{}': {source}", path.display())
            }
            Self::Io { path, source } => {
                write!(formatter, "I/O error for '{}': {source}", path.display())
            }
            Self::Git { path, message } => {
                write!(
                    formatter,
                    "could not identify Git revision for '{}': {message}",
                    path.display()
                )
            }
            Self::DirtySource(path) => write!(
                formatter,
                "tracked dictionary XML under '{}' has staged or unstaged changes",
                path.display()
            ),
            Self::InvalidOutput { path, reason } => {
                write!(formatter, "invalid output '{}': {reason}", path.display())
            }
            Self::UnsafeOutput { source, output } => write!(
                formatter,
                "output '{}' must not contain or be contained by source '{}'",
                output.display(),
                source.display()
            ),
            Self::ExistingOutput(path) => write!(
                formatter,
                "output '{}' already exists; pass --overwrite to replace both reports",
                path.display()
            ),
            Self::Serialization(error) => write!(formatter, "could not serialize report: {error}"),
            Self::InvalidSourceShape { path, reason } => {
                write!(
                    formatter,
                    "invalid dictionary structure in '{}': {reason}",
                    path.display()
                )
            }
        }
    }
}

impl Error for WebSourceAuditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(error) => Some(error),
            Self::Source { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::Serialization(error) => Some(error),
            Self::Git { .. }
            | Self::DirtySource(_)
            | Self::InvalidOutput { .. }
            | Self::UnsafeOutput { .. }
            | Self::ExistingOutput(_)
            | Self::InvalidSourceShape { .. } => None,
        }
    }
}

impl From<catalog::CatalogError> for WebSourceAuditError {
    fn from(error: catalog::CatalogError) -> Self {
        Self::Catalog(error)
    }
}

impl From<serde_json::Error> for WebSourceAuditError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Entry,
    PartOfSpeech,
    CommonPattern,
    Sense,
}

impl EntityKind {
    const ALL: [Self; 4] = [
        Self::Entry,
        Self::PartOfSpeech,
        Self::CommonPattern,
        Self::Sense,
    ];

    const fn key(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::PartOfSpeech => "part_of_speech",
            Self::CommonPattern => "common_pattern",
            Self::Sense => "sense",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationStatus {
    Resolved,
    SelfReference,
    Unresolved,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NamespaceKey {
    dictionary: Dictionary,
    kind: EntityKind,
}

impl NamespaceKey {
    const fn new(dictionary: Dictionary, kind: EntityKind) -> Self {
        Self { dictionary, kind }
    }

    fn label(&self) -> String {
        format!("{}:{}", self.dictionary.key(), self.kind.key())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CompactLocator {
    file_index: u16,
    entry_ordinal: u32,
    part_of_speech_ordinal: Option<u32>,
    common_pattern_ordinal: Option<u32>,
    sense_ordinal: Option<u32>,
}

impl CompactLocator {
    const fn entry(file_index: u16, entry_ordinal: u32) -> Self {
        Self {
            file_index,
            entry_ordinal,
            part_of_speech_ordinal: None,
            common_pattern_ordinal: None,
            sense_ordinal: None,
        }
    }

    fn with_kind(self, kind: EntityKind, ordinal: u32) -> Self {
        let mut locator = self;
        match kind {
            EntityKind::Entry => {}
            EntityKind::PartOfSpeech => locator.part_of_speech_ordinal = Some(ordinal),
            EntityKind::CommonPattern => locator.common_pattern_ordinal = Some(ordinal),
            EntityKind::Sense => locator.sense_ordinal = Some(ordinal),
        }
        locator
    }
}

#[derive(Debug, Clone)]
struct EntityData {
    kind: EntityKind,
    locator: CompactLocator,
    key: Option<String>,
}

#[derive(Debug, Clone)]
struct EntryData {
    dictionary: Dictionary,
    locator: CompactLocator,
    headword: Option<String>,
    homonym: Option<String>,
    entities: Vec<EntityData>,
    relations: Vec<RelationBuilder>,
}

#[derive(Debug, Clone, Default)]
struct RawFields {
    relation_types: Vec<String>,
    target_keys: Vec<String>,
    words: Vec<String>,
    homonyms: Vec<String>,
    units: Vec<String>,
    urls: Vec<String>,
}

#[derive(Debug, Clone)]
struct RelationBuilder {
    source_locator: CompactLocator,
    relation_ordinal: u32,
    fields: RawFields,
}

#[derive(Debug, Clone)]
struct RawRelation {
    dictionary: Dictionary,
    locator: CompactLocator,
    relation_ordinal: u32,
    source_entity: EntityData,
    source_entry: EntityData,
    source_headword: Option<String>,
    fields: RawFields,
    source_entity_occurrences_in_entry: u32,
}

#[derive(Debug, Clone, Copy)]
struct OccurrenceSummary {
    count: u32,
    first: CompactLocator,
}

#[derive(Debug, Default)]
struct KeyTracker {
    total: u64,
    missing: u64,
    malformed: u64,
    counts: HashMap<String, OccurrenceSummary>,
    duplicate_samples: HashMap<String, Vec<CompactLocator>>,
}

impl KeyTracker {
    fn add(&mut self, key: Option<&str>, locator: CompactLocator) {
        self.total += 1;
        let Some(key) = key.map(str::trim).filter(|key| !key.is_empty()) else {
            self.missing += 1;
            return;
        };
        if !key.bytes().all(|byte| byte.is_ascii_digit()) {
            self.malformed += 1;
        }
        match self.counts.get_mut(key) {
            Some(summary) => {
                summary.count += 1;
                let samples = self
                    .duplicate_samples
                    .entry(key.to_owned())
                    .or_insert_with(|| vec![summary.first]);
                if samples.len() < DUPLICATE_SAMPLE_LIMIT {
                    samples.push(locator);
                }
            }
            None => {
                self.counts.insert(
                    key.to_owned(),
                    OccurrenceSummary {
                        count: 1,
                        first: locator,
                    },
                );
            }
        }
    }

    fn count(&self, key: Option<&str>) -> u32 {
        key.map(str::trim)
            .and_then(|key| self.counts.get(key))
            .map_or(0, |summary| summary.count)
    }
}

#[derive(Debug, Default)]
struct ScopeAggregate {
    scopes: u64,
    scopes_with_duplicates: u64,
    duplicate_values: u64,
    duplicate_occurrences: u64,
    max_occurrences: u32,
}

impl ScopeAggregate {
    fn observe(&mut self, counts: &HashMap<String, u32>) {
        self.scopes += 1;
        let mut has_duplicate = false;
        for count in counts.values().copied().filter(|count| *count > 1) {
            has_duplicate = true;
            self.duplicate_values += 1;
            self.duplicate_occurrences += u64::from(count - 1);
            self.max_occurrences = self.max_occurrences.max(count);
        }
        if has_duplicate {
            self.scopes_with_duplicates += 1;
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub schema: &'static str,
    pub analyzer_version: &'static str,
    pub parent_commit: String,
    pub source_commit: String,
    pub input_files: Vec<InputFileReport>,
    pub dictionary_entry_counts: BTreeMap<String, u64>,
    pub namespaces: Vec<NamespaceReport>,
    pub cross_namespace_numeric_collisions: Vec<NumericCollisionReport>,
    pub relation_type_counts: BTreeMap<String, BTreeMap<String, u64>>,
    pub relation_status_counts: BTreeMap<String, StatusCounts>,
    pub cycle_summary: CycleSummary,
    pub canonical_namespace: CanonicalNamespaceReport,
    pub relations: Vec<RelationReport>,
}

#[derive(Debug, Serialize)]
pub struct InputFileReport {
    pub dictionary: String,
    pub volume_number: usize,
    pub volume_total: usize,
    pub relative_path: String,
}

#[derive(Debug, Serialize)]
pub struct NamespaceReport {
    pub dictionary: String,
    pub entity_kind: EntityKind,
    pub total_entities: u64,
    pub present_keys: u64,
    pub distinct_keys: u64,
    pub missing_keys: u64,
    pub malformed_keys: u64,
    pub globally_unique: bool,
    pub global_duplicate_values: u64,
    pub global_duplicate_occurrences: u64,
    pub global_max_occurrences: u32,
    pub within_file: ScopeReport,
    pub within_entry: ScopeReport,
    pub duplicate_groups: Vec<DuplicateGroupReport>,
}

#[derive(Debug, Serialize)]
pub struct ScopeReport {
    pub scopes: u64,
    pub scopes_with_duplicates: u64,
    pub duplicate_values: u64,
    pub duplicate_occurrences: u64,
    pub max_occurrences: u32,
}

#[derive(Debug, Serialize)]
pub struct DuplicateGroupReport {
    pub key: String,
    pub occurrences: u32,
    pub sample_locators: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct NumericCollisionReport {
    pub left_namespace: String,
    pub right_namespace: String,
    pub shared_numeric_values: u64,
    pub sample_values: Vec<u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct StatusCounts {
    pub total: u64,
    pub resolved: u64,
    pub self_reference: u64,
    pub unresolved: u64,
    pub ambiguous: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct CycleSummary {
    pub groups: u64,
    pub member_entries: u64,
    pub relation_edges: u64,
    pub details: Vec<CycleGroupReport>,
}

#[derive(Debug, Serialize)]
pub struct CycleGroupReport {
    pub member_count: usize,
    pub relation_edges: u64,
    pub sample_members: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CanonicalNamespaceReport {
    pub schema: &'static str,
    pub corpus_scope: &'static str,
    pub base_tuple: [&'static str; 4],
    pub sense_scope: &'static str,
    pub duplicate_policy: &'static str,
    pub relation_policy: &'static str,
    pub cross_corpus_policy: &'static str,
}

#[derive(Debug, Serialize)]
pub struct RelationReport {
    pub dictionary: String,
    pub locator: String,
    pub source_entity_id: String,
    pub source_entry_id: String,
    pub source_headword: Option<String>,
    pub raw_types: Vec<String>,
    pub raw_target_keys: Vec<String>,
    pub raw_words: Vec<String>,
    pub raw_homonyms: Vec<String>,
    pub raw_units: Vec<String>,
    pub raw_urls: Vec<String>,
    pub target_namespace: Option<String>,
    pub candidate_target_ids: Vec<String>,
    pub resolved_target_id: Option<String>,
    pub resolved_target_entry_id: Option<String>,
    pub status: RelationStatus,
    pub reason: String,
    pub in_cycle: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    canonical_id: String,
    entry_canonical_id: String,
    entry_key: Option<String>,
    headword: Option<String>,
    homonym: Option<String>,
}

#[derive(Debug, Default)]
struct FirstPass {
    trackers: BTreeMap<NamespaceKey, KeyTracker>,
    within_file: BTreeMap<NamespaceKey, ScopeAggregate>,
    within_entry: BTreeMap<NamespaceKey, ScopeAggregate>,
    relations: Vec<RawRelation>,
}

pub fn run_audit(
    source: &Path,
    output: &Path,
    overwrite: bool,
) -> Result<AuditReport, WebSourceAuditError> {
    let source = fs::canonicalize(source).map_err(|error| WebSourceAuditError::Io {
        path: source.to_path_buf(),
        source: error,
    })?;
    ensure_source_xml_clean(&source)?;
    let output = resolve_output(output)?;
    validate_output_boundary(&source, &output)?;

    let volumes = catalog::discover(&source, &Dictionary::ALL)?;
    let files = volumes
        .iter()
        .map(|volume| volume.relative_source.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let source_commit = git_revision(&source)?;
    let parent_commit =
        git_revision(
            &std::env::current_dir().map_err(|error| WebSourceAuditError::Io {
                path: PathBuf::from("."),
                source: error,
            })?,
        )?;

    let json_path = output.join(JSON_FILENAME);
    let markdown_path = output.join(MARKDOWN_FILENAME);
    if !overwrite {
        if json_path.exists() {
            return Err(WebSourceAuditError::ExistingOutput(json_path));
        }
        if markdown_path.exists() {
            return Err(WebSourceAuditError::ExistingOutput(markdown_path));
        }
    }

    let mut first = first_pass(&volumes, &files)?;
    let requested = requested_namespaces(&first.relations);
    let candidates = second_pass(
        &volumes,
        &files,
        &first.trackers,
        &requested,
        &source_commit,
    )?;
    let mut relations = resolve_relations(
        &first.relations,
        &files,
        &first.trackers,
        &candidates,
        &source_commit,
    );
    let cycle_summary = mark_cycles(&mut relations);
    let namespaces = build_namespace_reports(&first, &files);
    let cross_namespace_numeric_collisions = numeric_collisions(&first.trackers);
    let relation_type_counts = relation_type_counts(&first.relations);
    let relation_status_counts = relation_status_counts(&relations);
    let dictionary_entry_counts = dictionary_entry_counts(&first.trackers);

    let report = AuditReport {
        schema: REPORT_SCHEMA,
        analyzer_version: env!("CARGO_PKG_VERSION"),
        parent_commit,
        source_commit,
        input_files: input_file_reports(&volumes),
        dictionary_entry_counts,
        namespaces,
        cross_namespace_numeric_collisions,
        relation_type_counts,
        relation_status_counts,
        cycle_summary,
        canonical_namespace: CanonicalNamespaceReport {
            schema: "kweb-canonical-id-v1",
            corpus_scope: "the source submodule Git commit is part of identity scope",
            base_tuple: ["corpus_commit", "dictionary", "entity_kind", "native_key"],
            sense_scope: "sense identifiers are nested below their owning entry",
            duplicate_policy: "append relative XML path and source ordinal; never merge duplicate native keys",
            relation_policy: "preserve raw target fields separately from a uniquely resolved canonical target",
            cross_corpus_policy: "no continuity across corpus commits is inferred",
        },
        relations,
    };

    fs::create_dir_all(&output).map_err(|error| WebSourceAuditError::Io {
        path: output.clone(),
        source: error,
    })?;
    write_reports(&json_path, &markdown_path, &report)?;
    first.relations.clear();
    Ok(report)
}

fn first_pass(volumes: &[Volume], files: &[String]) -> Result<FirstPass, WebSourceAuditError> {
    let mut pass = FirstPass::default();
    for dictionary in Dictionary::ALL {
        for kind in EntityKind::ALL {
            let namespace = NamespaceKey::new(dictionary, kind);
            pass.trackers.entry(namespace.clone()).or_default();
            pass.within_file.entry(namespace.clone()).or_default();
            pass.within_entry.entry(namespace).or_default();
        }
    }

    for (file_index, volume) in volumes.iter().enumerate() {
        let file_index = u16::try_from(file_index).expect("124 source files fit in u16");
        let mut file_counts: BTreeMap<EntityKind, HashMap<String, u32>> = EntityKind::ALL
            .into_iter()
            .map(|kind| (kind, HashMap::new()))
            .collect();
        parse_volume(volume, file_index, |entry| {
            observe_entry(&mut pass, entry, &mut file_counts)
        })?;

        for kind in EntityKind::ALL {
            let namespace = NamespaceKey::new(volume.dictionary, kind);
            pass.within_file
                .get_mut(&namespace)
                .expect("namespace was initialized")
                .observe(file_counts.get(&kind).expect("kind was initialized"));
        }
    }

    if volumes.len() != files.len() {
        return Err(WebSourceAuditError::InvalidSourceShape {
            path: PathBuf::from("references/korean-dict-nikl"),
            reason: "internal source file index mismatch".to_owned(),
        });
    }
    Ok(pass)
}

fn observe_entry(
    pass: &mut FirstPass,
    entry: EntryData,
    file_counts: &mut BTreeMap<EntityKind, HashMap<String, u32>>,
) {
    let mut within_entry: BTreeMap<EntityKind, HashMap<String, u32>> = EntityKind::ALL
        .into_iter()
        .map(|kind| (kind, HashMap::new()))
        .collect();

    for entity in &entry.entities {
        let namespace = NamespaceKey::new(entry.dictionary, entity.kind);
        pass.trackers
            .get_mut(&namespace)
            .expect("namespace was initialized")
            .add(entity.key.as_deref(), entity.locator);
        if let Some(key) = normalized_key(entity.key.as_deref()) {
            *file_counts
                .get_mut(&entity.kind)
                .expect("kind was initialized")
                .entry(key.to_owned())
                .or_default() += 1;
            *within_entry
                .get_mut(&entity.kind)
                .expect("kind was initialized")
                .entry(key.to_owned())
                .or_default() += 1;
        }
    }

    for kind in EntityKind::ALL {
        let namespace = NamespaceKey::new(entry.dictionary, kind);
        pass.within_entry
            .get_mut(&namespace)
            .expect("namespace was initialized")
            .observe(within_entry.get(&kind).expect("kind was initialized"));
    }

    let source_entry = entry
        .entities
        .iter()
        .find(|entity| entity.kind == EntityKind::Entry)
        .cloned()
        .expect("every parsed entry has an entry entity");
    for relation in entry.relations {
        let source_entity = entry
            .entities
            .iter()
            .find(|entity| entity.locator == relation.source_locator)
            .cloned()
            .unwrap_or_else(|| source_entry.clone());
        let source_entity_occurrences_in_entry = source_entity
            .key
            .as_deref()
            .and_then(|key| normalized_key(Some(key)))
            .and_then(|key| within_entry.get(&source_entity.kind)?.get(key).copied())
            .unwrap_or(0);
        pass.relations.push(RawRelation {
            dictionary: entry.dictionary,
            locator: entry.locator,
            relation_ordinal: relation.relation_ordinal,
            source_entity,
            source_entry: source_entry.clone(),
            source_headword: entry.headword.clone(),
            fields: relation.fields,
            source_entity_occurrences_in_entry,
        });
    }
}

fn parse_volume<F>(
    volume: &Volume,
    file_index: u16,
    mut on_entry: F,
) -> Result<(), WebSourceAuditError>
where
    F: FnMut(EntryData),
{
    let input = File::open(&volume.source).map_err(|error| WebSourceAuditError::Io {
        path: volume.source.clone(),
        source: error,
    })?;
    let mut builder: Option<EntryBuilder> = None;
    let mut entry_ordinal = 0_u32;

    for record in SourceRecordReader::new(input) {
        let record = record.map_err(|error| WebSourceAuditError::Source {
            path: volume.source.clone(),
            source: error,
        })?;
        match record {
            SourceRecord::StartElement {
                depth,
                name,
                attributes,
            } => {
                let name = local_name(&name).to_owned();
                if builder.is_none() && volume.dictionary.is_entry_element(&name) {
                    entry_ordinal += 1;
                    builder = Some(EntryBuilder::new(
                        volume.dictionary,
                        CompactLocator::entry(file_index, entry_ordinal),
                        depth,
                        name,
                        &attributes,
                    ));
                } else if let Some(builder) = builder.as_mut() {
                    builder.start(name, &attributes);
                }
            }
            SourceRecord::EmptyElement {
                name, attributes, ..
            } => {
                if let Some(builder) = builder.as_mut() {
                    builder.empty(local_name(&name), &attributes);
                }
            }
            SourceRecord::ElementText { value, .. } => {
                if let Some(builder) = builder.as_mut() {
                    builder.text(&value);
                }
            }
            SourceRecord::TailText { .. } => {}
            SourceRecord::EndElement { depth, name } => {
                let name = local_name(&name);
                let finishes_entry = builder.as_ref().is_some_and(|builder| {
                    depth == builder.root_depth && name == builder.root_name
                });
                if finishes_entry {
                    let completed = builder.take().expect("entry builder exists").finish();
                    on_entry(completed);
                } else if let Some(builder) = builder.as_mut() {
                    builder.end(name);
                }
            }
        }
    }

    if builder.is_some() {
        return Err(WebSourceAuditError::InvalidSourceShape {
            path: volume.source.clone(),
            reason: "source ended inside an entry".to_owned(),
        });
    }
    Ok(())
}

struct EntryBuilder {
    dictionary: Dictionary,
    root_depth: usize,
    root_name: String,
    path: Vec<String>,
    data: EntryData,
    part_of_speech_count: u32,
    common_pattern_count: u32,
    sense_count: u32,
    relation_count: u32,
    current_part_of_speech: Option<CompactLocator>,
    current_common_pattern: Option<CompactLocator>,
    current_sense: Option<CompactLocator>,
    current_relation: Option<RelationBuilder>,
}

impl EntryBuilder {
    fn new(
        dictionary: Dictionary,
        locator: CompactLocator,
        root_depth: usize,
        root_name: String,
        attributes: &[SourceAttribute],
    ) -> Self {
        let entry_key = match dictionary {
            Dictionary::Krdict => lmf_identifier(attributes),
            Dictionary::Stdict | Dictionary::Opendict => attribute(attributes, "target_code"),
        };
        Self {
            dictionary,
            root_depth,
            root_name: root_name.clone(),
            path: vec![root_name],
            data: EntryData {
                dictionary,
                locator,
                headword: None,
                homonym: None,
                entities: vec![EntityData {
                    kind: EntityKind::Entry,
                    locator,
                    key: entry_key,
                }],
                relations: Vec::new(),
            },
            part_of_speech_count: 0,
            common_pattern_count: 0,
            sense_count: 0,
            relation_count: 0,
            current_part_of_speech: None,
            current_common_pattern: None,
            current_sense: None,
            current_relation: None,
        }
    }

    fn start(&mut self, name: String, attributes: &[SourceAttribute]) {
        self.path.push(name.clone());
        match (self.dictionary, name.as_str()) {
            (Dictionary::Krdict, "Sense") => {
                self.sense_count += 1;
                let locator = self
                    .data
                    .locator
                    .with_kind(EntityKind::Sense, self.sense_count);
                self.current_sense = Some(locator);
                self.data.entities.push(EntityData {
                    kind: EntityKind::Sense,
                    locator,
                    key: lmf_identifier(attributes),
                });
            }
            (Dictionary::Stdict, "pos_info") => {
                self.part_of_speech_count += 1;
                let locator = self
                    .data
                    .locator
                    .with_kind(EntityKind::PartOfSpeech, self.part_of_speech_count);
                self.current_part_of_speech = Some(locator);
                self.data.entities.push(EntityData {
                    kind: EntityKind::PartOfSpeech,
                    locator,
                    key: None,
                });
            }
            (Dictionary::Stdict, "comm_pattern_info") => {
                self.common_pattern_count += 1;
                let mut locator = self
                    .data
                    .locator
                    .with_kind(EntityKind::CommonPattern, self.common_pattern_count);
                locator.part_of_speech_ordinal = self
                    .current_part_of_speech
                    .and_then(|value| value.part_of_speech_ordinal);
                self.current_common_pattern = Some(locator);
                self.data.entities.push(EntityData {
                    kind: EntityKind::CommonPattern,
                    locator,
                    key: None,
                });
            }
            (Dictionary::Stdict, "sense_info") | (Dictionary::Opendict, "senseInfo") => {
                self.sense_count += 1;
                let mut locator = self
                    .data
                    .locator
                    .with_kind(EntityKind::Sense, self.sense_count);
                locator.part_of_speech_ordinal = self
                    .current_part_of_speech
                    .and_then(|value| value.part_of_speech_ordinal);
                locator.common_pattern_ordinal = self
                    .current_common_pattern
                    .and_then(|value| value.common_pattern_ordinal);
                self.current_sense = Some(locator);
                self.data.entities.push(EntityData {
                    kind: EntityKind::Sense,
                    locator,
                    key: None,
                });
            }
            _ => {}
        }

        if self.is_relation_container(&name) {
            self.relation_count += 1;
            self.current_relation = Some(RelationBuilder {
                source_locator: self.nearest_entity_locator(),
                relation_ordinal: self.relation_count,
                fields: RawFields::default(),
            });
        }
    }

    fn empty(&mut self, name: &str, attributes: &[SourceAttribute]) {
        if self.dictionary != Dictionary::Krdict || name != "feat" {
            return;
        }
        let Some(field) = attribute(attributes, "att") else {
            return;
        };
        let value = attribute(attributes, "val").unwrap_or_default();
        if let Some(relation) = self.current_relation.as_mut() {
            if self.path.last().is_some_and(|name| name == "SenseRelation") {
                match field.as_str() {
                    "type" => relation.fields.relation_types.push(value),
                    "id" => relation.fields.target_keys.push(value),
                    "lemma" => relation.fields.words.push(value),
                    "homonymNumber" => relation.fields.homonyms.push(value),
                    _ => {}
                }
                return;
            }
        }
        if field == "writtenForm" && self.path.iter().any(|name| name == "Lemma") {
            set_once(&mut self.data.headword, value);
        } else if field == "homonym_number" && self.path.len() == 1 {
            set_once(&mut self.data.homonym, value);
        }
    }

    fn text(&mut self, value: &str) {
        let Some(name) = self.path.last().map(String::as_str) else {
            return;
        };
        if let Some(relation) = self.current_relation.as_mut() {
            match name {
                "type" => relation.fields.relation_types.push(value.to_owned()),
                "link_target_code" => relation.fields.target_keys.push(value.to_owned()),
                "word" => relation.fields.words.push(value.to_owned()),
                "unit" => relation.fields.units.push(value.to_owned()),
                "link" => relation.fields.urls.push(value.to_owned()),
                _ => {}
            }
            return;
        }

        match (self.dictionary, name) {
            (Dictionary::Stdict | Dictionary::Opendict, "target_code") if self.path.len() == 2 => {
                self.set_entity_key(self.data.locator, value);
            }
            (Dictionary::Stdict, "pos_code") => {
                if let Some(locator) = self.current_part_of_speech {
                    self.set_entity_key(locator, value);
                }
            }
            (Dictionary::Stdict, "comm_pattern_code") => {
                if let Some(locator) = self.current_common_pattern {
                    self.set_entity_key(locator, value);
                }
            }
            (Dictionary::Stdict, "sense_code") | (Dictionary::Opendict, "sense_no") => {
                if let Some(locator) = self.current_sense {
                    self.set_entity_key(locator, value);
                }
            }
            (Dictionary::Stdict, "word") if self.path.iter().any(|name| name == "word_info") => {
                set_once(&mut self.data.headword, value.to_owned());
            }
            (Dictionary::Opendict, "word") if self.path.iter().any(|name| name == "wordInfo") => {
                set_once(&mut self.data.headword, value.to_owned());
            }
            (Dictionary::Opendict, "group_order") if self.path.len() == 2 => {
                set_once(&mut self.data.homonym, value.to_owned());
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        if self.is_relation_container(name) {
            if let Some(relation) = self.current_relation.take() {
                self.data.relations.push(relation);
            }
        }
        match (self.dictionary, name) {
            (Dictionary::Krdict, "Sense")
            | (Dictionary::Stdict, "sense_info")
            | (Dictionary::Opendict, "senseInfo") => self.current_sense = None,
            (Dictionary::Stdict, "comm_pattern_info") => self.current_common_pattern = None,
            (Dictionary::Stdict, "pos_info") => self.current_part_of_speech = None,
            _ => {}
        }
        self.path.pop();
    }

    fn finish(self) -> EntryData {
        self.data
    }

    fn is_relation_container(&self, name: &str) -> bool {
        matches!(
            (self.dictionary, name),
            (Dictionary::Krdict, "SenseRelation")
                | (Dictionary::Stdict, "lexical_info")
                | (Dictionary::Opendict, "relation_info")
        )
    }

    fn nearest_entity_locator(&self) -> CompactLocator {
        self.current_sense
            .or(self.current_common_pattern)
            .or(self.current_part_of_speech)
            .unwrap_or(self.data.locator)
    }

    fn set_entity_key(&mut self, locator: CompactLocator, value: &str) {
        if let Some(entity) = self
            .data
            .entities
            .iter_mut()
            .find(|entity| entity.locator == locator)
        {
            set_once(&mut entity.key, value.to_owned());
        }
    }
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

fn set_once(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

fn local_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit_once(':')
        .map_or(qualified_name, |(_, local)| local)
}

fn normalized_key(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn requested_namespaces(relations: &[RawRelation]) -> BTreeMap<NamespaceKey, HashSet<String>> {
    let mut requested = BTreeMap::new();
    for relation in relations {
        let target_keys = relation
            .fields
            .target_keys
            .iter()
            .filter_map(|key| normalized_key(Some(key)))
            .collect::<Vec<_>>();
        for key in target_keys {
            let kinds = target_kinds(relation.dictionary, single(&relation.fields.units));
            for kind in kinds {
                requested
                    .entry(NamespaceKey::new(relation.dictionary, kind))
                    .or_insert_with(HashSet::new)
                    .insert(key.to_owned());
            }
        }
    }
    requested
}

fn target_kinds(dictionary: Dictionary, unit: Option<&str>) -> Vec<EntityKind> {
    match dictionary {
        Dictionary::Krdict | Dictionary::Opendict => vec![EntityKind::Entry],
        Dictionary::Stdict => match unit.map(str::trim) {
            Some("어휘") => vec![EntityKind::Entry],
            Some("품사") => vec![EntityKind::PartOfSpeech],
            Some("공통 문형") => vec![EntityKind::CommonPattern],
            Some("의미") => vec![EntityKind::Sense],
            _ => EntityKind::ALL.to_vec(),
        },
    }
}

fn second_pass(
    volumes: &[Volume],
    files: &[String],
    trackers: &BTreeMap<NamespaceKey, KeyTracker>,
    requested: &BTreeMap<NamespaceKey, HashSet<String>>,
    source_commit: &str,
) -> Result<BTreeMap<(NamespaceKey, String), Vec<Candidate>>, WebSourceAuditError> {
    let mut candidates: BTreeMap<(NamespaceKey, String), Vec<Candidate>> = BTreeMap::new();
    for (file_index, volume) in volumes.iter().enumerate() {
        let file_index = u16::try_from(file_index).expect("124 source files fit in u16");
        parse_volume(volume, file_index, |entry| {
            collect_candidates(
                &entry,
                files,
                trackers,
                requested,
                source_commit,
                &mut candidates,
            );
        })?;
    }
    for values in candidates.values_mut() {
        values.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
    }
    Ok(candidates)
}

fn collect_candidates(
    entry: &EntryData,
    files: &[String],
    trackers: &BTreeMap<NamespaceKey, KeyTracker>,
    requested: &BTreeMap<NamespaceKey, HashSet<String>>,
    source_commit: &str,
    candidates: &mut BTreeMap<(NamespaceKey, String), Vec<Candidate>>,
) {
    let entry_entity = entry
        .entities
        .iter()
        .find(|entity| entity.kind == EntityKind::Entry)
        .expect("every entry has an entry entity");
    let entry_namespace = NamespaceKey::new(entry.dictionary, EntityKind::Entry);
    let entry_occurrences = trackers
        .get(&entry_namespace)
        .expect("entry tracker exists")
        .count(entry_entity.key.as_deref());
    let entry_id = canonical_id(
        source_commit,
        entry.dictionary,
        entry_entity,
        entry_occurrences,
        0,
        None,
        files,
    );

    let mut within_entry_counts: BTreeMap<(EntityKind, String), u32> = BTreeMap::new();
    for entity in &entry.entities {
        if let Some(key) = normalized_key(entity.key.as_deref()) {
            *within_entry_counts
                .entry((entity.kind, key.to_owned()))
                .or_default() += 1;
        }
    }

    for entity in &entry.entities {
        let Some(key) = normalized_key(entity.key.as_deref()) else {
            continue;
        };
        let namespace = NamespaceKey::new(entry.dictionary, entity.kind);
        if !requested
            .get(&namespace)
            .is_some_and(|values| values.contains(key))
        {
            continue;
        }
        let global_occurrences = trackers
            .get(&namespace)
            .expect("namespace tracker exists")
            .count(Some(key));
        let within_entry_occurrences = within_entry_counts
            .get(&(entity.kind, key.to_owned()))
            .copied()
            .unwrap_or(0);
        let canonical_id = if entity.kind == EntityKind::Entry {
            entry_id.clone()
        } else {
            canonical_id(
                source_commit,
                entry.dictionary,
                entity,
                global_occurrences,
                within_entry_occurrences,
                Some(&entry_id),
                files,
            )
        };
        candidates
            .entry((namespace, key.to_owned()))
            .or_default()
            .push(Candidate {
                canonical_id,
                entry_canonical_id: entry_id.clone(),
                entry_key: normalized_key(entry_entity.key.as_deref()).map(str::to_owned),
                headword: entry.headword.clone(),
                homonym: entry.homonym.clone(),
            });
    }
}

fn canonical_id(
    source_commit: &str,
    dictionary: Dictionary,
    entity: &EntityData,
    global_occurrences: u32,
    within_entry_occurrences: u32,
    entry_id: Option<&str>,
    files: &[String],
) -> String {
    let key = normalized_key(entity.key.as_deref())
        .map(percent_encode)
        .unwrap_or_else(|| "missing".to_owned());
    let locator = format_locator(dictionary, entity.locator, files);
    if entity.kind == EntityKind::Entry {
        let mut value = format!(
            "kweb:v1/{}/{}/entry/{key}",
            percent_encode(source_commit),
            dictionary.key()
        );
        if global_occurrences != 1 {
            value.push_str("/at/");
            value.push_str(&percent_encode(&locator));
        }
        return value;
    }

    let mut value = format!(
        "{}/{}/{}",
        entry_id.expect("nested entities have an owning entry"),
        entity.kind.key(),
        key
    );
    if within_entry_occurrences != 1 {
        value.push_str("/at/");
        value.push_str(&percent_encode(&locator));
    }
    value
}

fn source_ids(
    relation: &RawRelation,
    trackers: &BTreeMap<NamespaceKey, KeyTracker>,
    source_commit: &str,
    files: &[String],
) -> (String, String) {
    let entry_namespace = NamespaceKey::new(relation.dictionary, EntityKind::Entry);
    let entry_occurrences = trackers
        .get(&entry_namespace)
        .expect("entry tracker exists")
        .count(relation.source_entry.key.as_deref());
    let entry_id = canonical_id(
        source_commit,
        relation.dictionary,
        &relation.source_entry,
        entry_occurrences,
        0,
        None,
        files,
    );
    if relation.source_entity.kind == EntityKind::Entry {
        return (entry_id.clone(), entry_id);
    }
    let namespace = NamespaceKey::new(relation.dictionary, relation.source_entity.kind);
    let global_occurrences = trackers
        .get(&namespace)
        .expect("namespace tracker exists")
        .count(relation.source_entity.key.as_deref());
    let entity_id = canonical_id(
        source_commit,
        relation.dictionary,
        &relation.source_entity,
        global_occurrences,
        relation.source_entity_occurrences_in_entry,
        Some(&entry_id),
        files,
    );
    (entity_id, entry_id)
}

fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(output, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    output
}

fn resolve_relations(
    relations: &[RawRelation],
    files: &[String],
    trackers: &BTreeMap<NamespaceKey, KeyTracker>,
    candidates: &BTreeMap<(NamespaceKey, String), Vec<Candidate>>,
    source_commit: &str,
) -> Vec<RelationReport> {
    relations
        .iter()
        .map(|relation| resolve_relation(relation, files, trackers, candidates, source_commit))
        .collect()
}

fn resolve_relation(
    relation: &RawRelation,
    files: &[String],
    trackers: &BTreeMap<NamespaceKey, KeyTracker>,
    candidates: &BTreeMap<(NamespaceKey, String), Vec<Candidate>>,
    source_commit: &str,
) -> RelationReport {
    let (source_entity_id, source_entry_id) = source_ids(relation, trackers, source_commit, files);
    let locator = format!(
        "{}#relation={}",
        format_locator(relation.dictionary, relation.locator, files),
        relation.relation_ordinal
    );
    let unit = single(&relation.fields.units);
    let kinds = target_kinds(relation.dictionary, unit);
    let target_namespace =
        (kinds.len() == 1).then(|| NamespaceKey::new(relation.dictionary, kinds[0]).label());
    let target_key =
        single(&relation.fields.target_keys).and_then(|value| normalized_key(Some(value)));
    let mut all_candidates = Vec::new();
    if let Some(key) = target_key {
        for kind in &kinds {
            let namespace = NamespaceKey::new(relation.dictionary, *kind);
            if let Some(values) = candidates.get(&(namespace, key.to_owned())) {
                all_candidates.extend(values.iter());
            }
        }
    }
    all_candidates.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
    all_candidates.dedup_by(|left, right| left.canonical_id == right.canonical_id);
    let candidate_target_ids = all_candidates
        .iter()
        .map(|candidate| candidate.canonical_id.clone())
        .collect::<Vec<_>>();
    let mut eligible = all_candidates;
    let mut conflicts = Vec::new();

    if relation.fields.relation_types.len() != 1 {
        conflicts.push("relation type is missing or repeated".to_owned());
    }
    if relation.fields.target_keys.len() > 1 {
        conflicts.push("target key is repeated".to_owned());
    }
    if relation.dictionary == Dictionary::Stdict && relation.fields.units.len() != 1 {
        conflicts
            .push("standard-dictionary target unit is missing, repeated, or unknown".to_owned());
    }
    if relation.fields.urls.len() > 1 {
        conflicts.push("target URL is repeated".to_owned());
    }
    if relation.fields.words.len() > 1 || relation.fields.homonyms.len() > 1 {
        conflicts.push("target label discriminator is repeated".to_owned());
    }
    if target_key.is_none() {
        eligible.clear();
    }

    if let Some(url) = single(&relation.fields.urls) {
        let parsed = ParsedUrl::parse(url);
        let expected_host = match relation.dictionary {
            Dictionary::Krdict => "krdict.korean.go.kr",
            Dictionary::Stdict => "stdict.korean.go.kr",
            Dictionary::Opendict => "opendict.korean.go.kr",
        };
        if parsed.host.as_deref() != Some(expected_host) {
            conflicts.push(format!("URL host does not match {expected_host}"));
        }
        match relation.dictionary {
            Dictionary::Krdict => {}
            Dictionary::Stdict => match parsed.query.get("word_no") {
                Some(word_no) => {
                    let matching = eligible
                        .iter()
                        .copied()
                        .filter(|candidate| candidate.entry_key.as_deref() == Some(word_no.trim()))
                        .collect::<Vec<_>>();
                    if !eligible.is_empty() && matching.is_empty() {
                        conflicts
                            .push("URL word_no conflicts with target candidate owner".to_owned());
                    } else if !matching.is_empty() {
                        eligible = matching;
                    }
                }
                None => conflicts.push("standard-dictionary URL has no word_no".to_owned()),
            },
            Dictionary::Opendict => match (parsed.query.get("sense_no"), target_key) {
                (Some(url_key), Some(key)) if url_key.trim() == key => {}
                (Some(_), Some(_)) => conflicts
                    .push("open-dictionary URL sense_no conflicts with target key".to_owned()),
                (None, _) => conflicts.push("open-dictionary URL has no sense_no".to_owned()),
                _ => {}
            },
        }
    }

    if let Some(word) = single(&relation.fields.words) {
        let matching = eligible
            .iter()
            .copied()
            .filter(|candidate| {
                candidate
                    .headword
                    .as_deref()
                    .is_some_and(|headword| labels_compatible(word, headword))
            })
            .collect::<Vec<_>>();
        if !eligible.is_empty() && matching.is_empty() {
            conflicts.push("relation word conflicts with all keyed candidates".to_owned());
        } else if !matching.is_empty() {
            eligible = matching;
        }
    }

    if relation.dictionary == Dictionary::Krdict {
        if let Some(homonym) = single(&relation.fields.homonyms) {
            let matching = eligible
                .iter()
                .copied()
                .filter(|candidate| {
                    candidate.homonym.as_deref().map(str::trim) == Some(homonym.trim())
                })
                .collect::<Vec<_>>();
            if !eligible.is_empty() && matching.is_empty() {
                conflicts
                    .push("relation homonym number conflicts with all keyed candidates".to_owned());
            } else if !matching.is_empty() {
                eligible = matching;
            }
        }
    }

    let (status, resolved, reason) = if !conflicts.is_empty() {
        (RelationStatus::Ambiguous, None, conflicts.join("; "))
    } else if target_key.is_none() {
        (
            RelationStatus::Unresolved,
            None,
            "relation has no usable target key".to_owned(),
        )
    } else {
        match eligible.as_slice() {
            [] => (
                RelationStatus::Unresolved,
                None,
                "no candidate exists in the declared dictionary and entity namespace".to_owned(),
            ),
            [candidate] if candidate.entry_canonical_id == source_entry_id => (
                RelationStatus::SelfReference,
                Some(*candidate),
                "unique target resolves to the owning source entry".to_owned(),
            ),
            [candidate] => (
                RelationStatus::Resolved,
                Some(*candidate),
                "one candidate matches key, target kind, URL, and available label discriminators"
                    .to_owned(),
            ),
            _ => (
                RelationStatus::Ambiguous,
                None,
                "multiple candidates remain after applying authoritative discriminators".to_owned(),
            ),
        }
    };

    RelationReport {
        dictionary: relation.dictionary.key().to_owned(),
        locator,
        source_entity_id,
        source_entry_id,
        source_headword: relation.source_headword.clone(),
        raw_types: relation.fields.relation_types.clone(),
        raw_target_keys: relation.fields.target_keys.clone(),
        raw_words: relation.fields.words.clone(),
        raw_homonyms: relation.fields.homonyms.clone(),
        raw_units: relation.fields.units.clone(),
        raw_urls: relation.fields.urls.clone(),
        target_namespace,
        candidate_target_ids,
        resolved_target_id: resolved.map(|candidate| candidate.canonical_id.clone()),
        resolved_target_entry_id: resolved.map(|candidate| candidate.entry_canonical_id.clone()),
        status,
        reason,
        in_cycle: false,
    }
}

fn single(values: &[String]) -> Option<&str> {
    (values.len() == 1).then(|| values[0].as_str())
}

fn labels_compatible(relation_word: &str, candidate_headword: &str) -> bool {
    let relation_word = relation_word.trim();
    let candidate_headword = candidate_headword.trim();
    if relation_word == candidate_headword {
        return true;
    }
    let suffix_digits = relation_word
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    matches!(suffix_digits, 2 | 3)
        && relation_word
            .get(..relation_word.len() - suffix_digits)
            .is_some_and(|value| value == candidate_headword)
}

#[derive(Debug, Default)]
struct ParsedUrl {
    host: Option<String>,
    query: BTreeMap<String, String>,
}

impl ParsedUrl {
    fn parse(value: &str) -> Self {
        let value = value.trim();
        let Some((_, remainder)) = value.split_once("://") else {
            return Self::default();
        };
        let host_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        let host = remainder[..host_end]
            .split('@')
            .next_back()
            .and_then(|host| host.split(':').next())
            .map(str::to_ascii_lowercase);
        let mut query = BTreeMap::new();
        if let Some((_, query_string)) = value.split_once('?') {
            for pair in query_string.split('&') {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                query.insert(name.to_owned(), value.to_owned());
            }
        }
        Self { host, query }
    }
}

fn build_namespace_reports(first: &FirstPass, files: &[String]) -> Vec<NamespaceReport> {
    first
        .trackers
        .iter()
        .map(|(namespace, tracker)| {
            let mut duplicate_groups = tracker
                .counts
                .iter()
                .filter(|(_, summary)| summary.count > 1)
                .map(|(key, summary)| {
                    let samples = tracker
                        .duplicate_samples
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| vec![summary.first]);
                    DuplicateGroupReport {
                        key: key.clone(),
                        occurrences: summary.count,
                        sample_locators: samples
                            .into_iter()
                            .map(|locator| format_locator(namespace.dictionary, locator, files))
                            .collect(),
                    }
                })
                .collect::<Vec<_>>();
            duplicate_groups.sort_by(|left, right| compare_native_keys(&left.key, &right.key));
            let global_duplicate_values = duplicate_groups.len() as u64;
            let global_duplicate_occurrences = duplicate_groups
                .iter()
                .map(|group| u64::from(group.occurrences - 1))
                .sum();
            let global_max_occurrences = duplicate_groups
                .iter()
                .map(|group| group.occurrences)
                .max()
                .unwrap_or(u32::from(tracker.total > 0));
            NamespaceReport {
                dictionary: namespace.dictionary.key().to_owned(),
                entity_kind: namespace.kind,
                total_entities: tracker.total,
                present_keys: tracker.total - tracker.missing,
                distinct_keys: tracker.counts.len() as u64,
                missing_keys: tracker.missing,
                malformed_keys: tracker.malformed,
                globally_unique: tracker.missing == 0 && global_duplicate_values == 0,
                global_duplicate_values,
                global_duplicate_occurrences,
                global_max_occurrences,
                within_file: scope_report(
                    first
                        .within_file
                        .get(namespace)
                        .expect("within-file scope exists"),
                ),
                within_entry: scope_report(
                    first
                        .within_entry
                        .get(namespace)
                        .expect("within-entry scope exists"),
                ),
                duplicate_groups,
            }
        })
        .collect()
}

fn scope_report(scope: &ScopeAggregate) -> ScopeReport {
    ScopeReport {
        scopes: scope.scopes,
        scopes_with_duplicates: scope.scopes_with_duplicates,
        duplicate_values: scope.duplicate_values,
        duplicate_occurrences: scope.duplicate_occurrences,
        max_occurrences: scope.max_occurrences,
    }
}

fn compare_native_keys(left: &str, right: &str) -> std::cmp::Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left_number), Ok(right_number)) => {
            left_number.cmp(&right_number).then_with(|| left.cmp(right))
        }
        _ => left.cmp(right),
    }
}

fn numeric_collisions(
    trackers: &BTreeMap<NamespaceKey, KeyTracker>,
) -> Vec<NumericCollisionReport> {
    let namespaces = trackers.keys().cloned().collect::<Vec<_>>();
    let mut reports = Vec::new();
    for left_index in 0..namespaces.len() {
        for right_index in (left_index + 1)..namespaces.len() {
            let left = &namespaces[left_index];
            let right = &namespaces[right_index];
            let left_values = trackers[left]
                .counts
                .keys()
                .filter_map(|key| key.parse::<u64>().ok())
                .collect::<HashSet<_>>();
            let right_values = trackers[right]
                .counts
                .keys()
                .filter_map(|key| key.parse::<u64>().ok())
                .collect::<BTreeSet<_>>();
            let shared = right_values
                .iter()
                .filter(|value| left_values.contains(value))
                .copied()
                .collect::<Vec<_>>();
            reports.push(NumericCollisionReport {
                left_namespace: left.label(),
                right_namespace: right.label(),
                shared_numeric_values: shared.len() as u64,
                sample_values: shared.into_iter().take(COLLISION_SAMPLE_LIMIT).collect(),
            });
        }
    }
    reports
}

fn relation_type_counts(relations: &[RawRelation]) -> BTreeMap<String, BTreeMap<String, u64>> {
    let mut counts: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for relation in relations {
        let value = match relation.fields.relation_types.as_slice() {
            [] => "(missing)".to_owned(),
            [value] => value.trim().to_owned(),
            values => format!("(multiple: {})", values.join(" | ")),
        };
        *counts
            .entry(relation.dictionary.key().to_owned())
            .or_default()
            .entry(value)
            .or_default() += 1;
    }
    counts
}

fn relation_status_counts(relations: &[RelationReport]) -> BTreeMap<String, StatusCounts> {
    let mut counts: BTreeMap<String, StatusCounts> = BTreeMap::new();
    for relation in relations {
        let dictionary = counts.entry(relation.dictionary.clone()).or_default();
        dictionary.total += 1;
        match relation.status {
            RelationStatus::Resolved => dictionary.resolved += 1,
            RelationStatus::SelfReference => dictionary.self_reference += 1,
            RelationStatus::Unresolved => dictionary.unresolved += 1,
            RelationStatus::Ambiguous => dictionary.ambiguous += 1,
        }
    }
    counts
}

fn dictionary_entry_counts(trackers: &BTreeMap<NamespaceKey, KeyTracker>) -> BTreeMap<String, u64> {
    Dictionary::ALL
        .into_iter()
        .map(|dictionary| {
            let namespace = NamespaceKey::new(dictionary, EntityKind::Entry);
            (
                dictionary.key().to_owned(),
                trackers
                    .get(&namespace)
                    .expect("entry tracker exists")
                    .total,
            )
        })
        .collect()
}

fn input_file_reports(volumes: &[Volume]) -> Vec<InputFileReport> {
    volumes
        .iter()
        .map(|volume| InputFileReport {
            dictionary: volume.dictionary.key().to_owned(),
            volume_number: volume.number,
            volume_total: volume.total,
            relative_path: volume.relative_source.to_string_lossy().into_owned(),
        })
        .collect()
}

fn mark_cycles(relations: &mut [RelationReport]) -> CycleSummary {
    let mut nodes = BTreeSet::new();
    for relation in relations.iter() {
        if matches!(relation.status, RelationStatus::Resolved) {
            nodes.insert(relation.source_entry_id.clone());
            if let Some(target) = &relation.resolved_target_entry_id {
                nodes.insert(target.clone());
            }
        }
    }
    let names = nodes.into_iter().collect::<Vec<_>>();
    let indexes = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut adjacency = vec![Vec::new(); names.len()];
    let mut reverse = vec![Vec::new(); names.len()];
    let mut relation_edges = Vec::new();
    for (relation_index, relation) in relations.iter().enumerate() {
        if !matches!(relation.status, RelationStatus::Resolved) {
            continue;
        }
        let Some(target_name) = &relation.resolved_target_entry_id else {
            continue;
        };
        let source = indexes[&relation.source_entry_id];
        let target = indexes[target_name];
        adjacency[source].push(target);
        reverse[target].push(source);
        relation_edges.push((relation_index, source, target));
    }
    for edges in &mut adjacency {
        edges.sort_unstable();
        edges.dedup();
    }
    for edges in &mut reverse {
        edges.sort_unstable();
        edges.dedup();
    }

    let order = finishing_order(&adjacency);
    let mut component = vec![usize::MAX; names.len()];
    let mut components = Vec::new();
    for start in order.into_iter().rev() {
        if component[start] != usize::MAX {
            continue;
        }
        let component_id = components.len();
        let mut members = Vec::new();
        let mut stack = vec![start];
        component[start] = component_id;
        while let Some(node) = stack.pop() {
            members.push(node);
            for &next in &reverse[node] {
                if component[next] == usize::MAX {
                    component[next] = component_id;
                    stack.push(next);
                }
            }
        }
        components.push(members);
    }

    let cyclic_components = components
        .iter()
        .enumerate()
        .filter(|(_, members)| members.len() >= 2)
        .map(|(index, _)| index)
        .collect::<HashSet<_>>();
    let mut edge_counts = vec![0_u64; components.len()];
    for (relation_index, source, target) in relation_edges {
        let component_id = component[source];
        if component_id == component[target] && cyclic_components.contains(&component_id) {
            relations[relation_index].in_cycle = true;
            edge_counts[component_id] += 1;
        }
    }

    let mut details = components
        .iter()
        .enumerate()
        .filter(|(index, members)| members.len() >= 2 && edge_counts[*index] > 0)
        .map(|(index, members)| {
            let mut member_names = members
                .iter()
                .map(|member| names[*member].clone())
                .collect::<Vec<_>>();
            member_names.sort();
            member_names.truncate(CYCLE_SAMPLE_LIMIT);
            CycleGroupReport {
                member_count: members.len(),
                relation_edges: edge_counts[index],
                sample_members: member_names,
            }
        })
        .collect::<Vec<_>>();
    details.sort_by(|left, right| left.sample_members.cmp(&right.sample_members));
    CycleSummary {
        groups: details.len() as u64,
        member_entries: details.iter().map(|group| group.member_count as u64).sum(),
        relation_edges: details.iter().map(|group| group.relation_edges).sum(),
        details,
    }
}

fn finishing_order(adjacency: &[Vec<usize>]) -> Vec<usize> {
    let mut visited = vec![false; adjacency.len()];
    let mut order = Vec::with_capacity(adjacency.len());
    for start in 0..adjacency.len() {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0_usize)];
        while let Some((node, next_index)) = stack.last_mut() {
            if *next_index < adjacency[*node].len() {
                let next = adjacency[*node][*next_index];
                *next_index += 1;
                if !visited[next] {
                    visited[next] = true;
                    stack.push((next, 0));
                }
            } else {
                let (finished, _) = stack.pop().expect("DFS stack is not empty");
                order.push(finished);
            }
        }
    }
    order
}

fn write_reports(
    json_path: &Path,
    markdown_path: &Path,
    report: &AuditReport,
) -> Result<(), WebSourceAuditError> {
    let mut json = AtomicWriteFile::options()
        .open(json_path)
        .map_err(|error| WebSourceAuditError::Io {
            path: json_path.to_path_buf(),
            source: error,
        })?;
    serde_json::to_writer_pretty(&mut json, report)?;
    json.write_all(b"\n")
        .map_err(|error| WebSourceAuditError::Io {
            path: json_path.to_path_buf(),
            source: error,
        })?;

    let markdown_text = markdown_report(report);
    let mut markdown = AtomicWriteFile::options()
        .open(markdown_path)
        .map_err(|error| WebSourceAuditError::Io {
            path: markdown_path.to_path_buf(),
            source: error,
        })?;
    markdown
        .write_all(markdown_text.as_bytes())
        .map_err(|error| WebSourceAuditError::Io {
            path: markdown_path.to_path_buf(),
            source: error,
        })?;

    json.commit().map_err(|error| WebSourceAuditError::Io {
        path: json_path.to_path_buf(),
        source: error,
    })?;
    markdown.commit().map_err(|error| WebSourceAuditError::Io {
        path: markdown_path.to_path_buf(),
        source: error,
    })?;
    Ok(())
}

fn markdown_report(report: &AuditReport) -> String {
    let mut output = String::new();
    use std::fmt::Write as _;
    writeln!(output, "# KWEB-002 source identifier and relation audit\n").unwrap();
    writeln!(output, "- report schema: `{}`", report.schema).unwrap();
    writeln!(output, "- parent commit: `{}`", report.parent_commit).unwrap();
    writeln!(output, "- source commit: `{}`", report.source_commit).unwrap();
    writeln!(
        output,
        "- tracked XML files: `{}`\n",
        report.input_files.len()
    )
    .unwrap();

    writeln!(output, "## Entry counts\n").unwrap();
    writeln!(output, "| dictionary | entries |").unwrap();
    writeln!(output, "| --- | ---: |").unwrap();
    for (dictionary, count) in &report.dictionary_entry_counts {
        writeln!(output, "| {} | {} |", markdown_cell(dictionary), count).unwrap();
    }
    writeln!(output).unwrap();

    writeln!(output, "## Identifier namespaces\n").unwrap();
    writeln!(output, "| dictionary | kind | total | distinct | missing | global duplicate values | within-entry duplicate values | max occurrences | unique |").unwrap();
    writeln!(
        output,
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |"
    )
    .unwrap();
    for namespace in &report.namespaces {
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            namespace.dictionary,
            namespace.entity_kind.key(),
            namespace.total_entities,
            namespace.distinct_keys,
            namespace.missing_keys,
            namespace.global_duplicate_values,
            namespace.within_entry.duplicate_values,
            namespace.global_max_occurrences,
            namespace.globally_unique
        )
        .unwrap();
    }
    writeln!(output).unwrap();

    writeln!(output, "## Cross-namespace numeric collisions\n").unwrap();
    writeln!(
        output,
        "| left namespace | right namespace | shared numeric values |"
    )
    .unwrap();
    writeln!(output, "| --- | --- | ---: |").unwrap();
    for collision in report
        .cross_namespace_numeric_collisions
        .iter()
        .filter(|collision| collision.shared_numeric_values > 0)
    {
        writeln!(
            output,
            "| {} | {} | {} |",
            collision.left_namespace, collision.right_namespace, collision.shared_numeric_values
        )
        .unwrap();
    }
    writeln!(output).unwrap();

    writeln!(output, "## Relation status\n").unwrap();
    writeln!(
        output,
        "| dictionary | total | resolved | self-reference | unresolved | ambiguous |"
    )
    .unwrap();
    writeln!(output, "| --- | ---: | ---: | ---: | ---: | ---: |").unwrap();
    for (dictionary, counts) in &report.relation_status_counts {
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} |",
            dictionary,
            counts.total,
            counts.resolved,
            counts.self_reference,
            counts.unresolved,
            counts.ambiguous
        )
        .unwrap();
    }
    writeln!(output).unwrap();

    writeln!(output, "## Relation types\n").unwrap();
    writeln!(output, "| dictionary | type | count |").unwrap();
    writeln!(output, "| --- | --- | ---: |").unwrap();
    for (dictionary, types) in &report.relation_type_counts {
        for (relation_type, count) in types {
            writeln!(
                output,
                "| {} | {} | {} |",
                dictionary,
                markdown_cell(relation_type),
                count
            )
            .unwrap();
        }
    }
    writeln!(output).unwrap();

    writeln!(output, "## Cycles\n").unwrap();
    writeln!(output, "- cycle groups: `{}`", report.cycle_summary.groups).unwrap();
    writeln!(
        output,
        "- member entries: `{}`",
        report.cycle_summary.member_entries
    )
    .unwrap();
    writeln!(
        output,
        "- relation edges in cycles: `{}`\n",
        report.cycle_summary.relation_edges
    )
    .unwrap();

    writeln!(output, "## Representative evidence\n").unwrap();
    for status in [
        RelationStatus::Resolved,
        RelationStatus::SelfReference,
        RelationStatus::Unresolved,
        RelationStatus::Ambiguous,
    ] {
        if let Some(relation) = report
            .relations
            .iter()
            .find(|relation| relation.status == status)
        {
            writeln!(
                output,
                "- `{:?}`: `{}` — {}",
                status, relation.locator, relation.reason
            )
            .unwrap();
        }
    }
    writeln!(output).unwrap();

    writeln!(output, "## Canonical namespace conclusion\n").unwrap();
    writeln!(output, "- schema: `{}`", report.canonical_namespace.schema).unwrap();
    writeln!(
        output,
        "- tuple: `corpus_commit + dictionary + entity_kind + native_key`"
    )
    .unwrap();
    writeln!(
        output,
        "- sense scope: {}",
        report.canonical_namespace.sense_scope
    )
    .unwrap();
    writeln!(
        output,
        "- duplicate policy: {}",
        report.canonical_namespace.duplicate_policy
    )
    .unwrap();
    writeln!(
        output,
        "- relation policy: {}",
        report.canonical_namespace.relation_policy
    )
    .unwrap();
    writeln!(
        output,
        "- cross-corpus policy: {}",
        report.canonical_namespace.cross_corpus_policy
    )
    .unwrap();
    output
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn format_locator(dictionary: Dictionary, locator: CompactLocator, files: &[String]) -> String {
    let mut value = format!(
        "{}:{}#entry={}",
        dictionary.key(),
        files
            .get(usize::from(locator.file_index))
            .map_or("<unknown>", String::as_str),
        locator.entry_ordinal
    );
    use std::fmt::Write as _;
    if let Some(ordinal) = locator.part_of_speech_ordinal {
        write!(value, "/pos={ordinal}").unwrap();
    }
    if let Some(ordinal) = locator.common_pattern_ordinal {
        write!(value, "/pattern={ordinal}").unwrap();
    }
    if let Some(ordinal) = locator.sense_ordinal {
        write!(value, "/sense={ordinal}").unwrap();
    }
    value
}

fn git_revision(path: &Path) -> Result<String, WebSourceAuditError> {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("-c")
        .arg(format!("safe.directory={}", path.to_string_lossy()))
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .map_err(|error| WebSourceAuditError::Git {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(WebSourceAuditError::Git {
            path: path.to_path_buf(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn ensure_source_xml_clean(path: &Path) -> Result<(), WebSourceAuditError> {
    let pathspecs = [
        ":(glob)krdict/*.xml",
        ":(glob)stdict/*.xml",
        ":(glob)opendict/*.xml",
    ];
    for staged in [false, true] {
        let mut command = Command::new("git");
        command
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("-c")
            .arg(format!("safe.directory={}", path.to_string_lossy()))
            .arg("-C")
            .arg(path)
            .arg("diff")
            .arg("--quiet");
        if staged {
            command.arg("--cached");
        }
        let status = command
            .arg("--")
            .args(pathspecs)
            .status()
            .map_err(|error| WebSourceAuditError::Git {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        match status.code() {
            Some(0) => {}
            Some(1) => return Err(WebSourceAuditError::DirtySource(path.to_path_buf())),
            _ => {
                return Err(WebSourceAuditError::Git {
                    path: path.to_path_buf(),
                    message: format!("git diff exited with {status}"),
                });
            }
        }
    }
    Ok(())
}

fn resolve_output(path: &Path) -> Result<PathBuf, WebSourceAuditError> {
    let absolute = if path.is_absolute() {
        clean_path(path)
    } else {
        clean_path(
            &std::env::current_dir()
                .map_err(|error| WebSourceAuditError::Io {
                    path: PathBuf::from("."),
                    source: error,
                })?
                .join(path),
        )
    };
    if absolute.exists() {
        let canonical =
            fs::canonicalize(&absolute).map_err(|error| WebSourceAuditError::InvalidOutput {
                path: absolute.clone(),
                reason: error.to_string(),
            })?;
        if !canonical.is_dir() {
            return Err(WebSourceAuditError::InvalidOutput {
                path: canonical,
                reason: "existing output is not a directory".to_owned(),
            });
        }
        return Ok(canonical);
    }
    canonicalize_with_missing_tail(&absolute).map_err(|reason| WebSourceAuditError::InvalidOutput {
        path: absolute,
        reason,
    })
}

fn validate_output_boundary(source: &Path, output: &Path) -> Result<(), WebSourceAuditError> {
    if source == output || source.starts_with(output) || output.starts_with(source) {
        return Err(WebSourceAuditError::UnsafeOutput {
            source: source.to_path_buf(),
            output: output.to_path_buf(),
        });
    }
    Ok(())
}

fn clean_path(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(cleaned.components().next_back(), Some(Component::Normal(_))) {
                    cleaned.pop();
                } else if !cleaned.has_root() {
                    cleaned.push(component.as_os_str());
                }
            }
            _ => cleaned.push(component.as_os_str()),
        }
    }
    cleaned
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path;
    let mut tail: Vec<OsString> = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| "no existing ancestor was found".to_owned())?;
        tail.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| "no existing ancestor was found".to_owned())?;
    }
    let mut resolved = fs::canonicalize(ancestor).map_err(|error| error.to_string())?;
    for component in tail.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{compare_native_keys, labels_compatible};

    #[test]
    fn numeric_key_order_uses_raw_text_as_a_stable_tie_break() {
        assert_eq!(compare_native_keys("001", "1"), Ordering::Less);
        assert_eq!(compare_native_keys("2", "10"), Ordering::Less);
        assert_eq!(compare_native_keys("x", "y"), Ordering::Less);
    }

    #[test]
    fn relation_labels_only_accept_exact_or_dictionary_homonym_suffixes() {
        assert!(labels_compatible("대상", "대상"));
        assert!(labels_compatible("대상01", "대상"));
        assert!(labels_compatible("대상001", "대상"));
        assert!(!labels_compatible("다른대상001", "대상"));
    }
}
