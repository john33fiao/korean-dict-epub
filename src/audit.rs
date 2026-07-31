use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str;

use atomic_write_file::AtomicWriteFile;
use quick_xml::XmlVersion;
use quick_xml::encoding::EncodingError;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::reader::Reader;
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zip::ZipArchive;

use crate::catalog::{Dictionary, Volume};

const AUDIT_SCHEMA: &str = "kdep-audit-report-v1";
const DIGEST_SCHEMA: &str = "kdep-source-record-v1";
const DIGEST_PREAMBLE: &[u8] = b"korean-dict-epub/source-record-digest/v1\0";
const BOOK_NAMESPACE: Uuid = Uuid::from_u128(0x34a87ea1f0e44f4d88941de178aa7a3e);
const MODIFIED_TIMESTAMP: &str = "1980-01-01T00:00:00Z";
const XML_BUFFER_CAPACITY: usize = 64 * 1024;
const SANITIZER_INPUT_CAPACITY: usize = 8 * 1024;
const AUDIT_CONTROL_ESCAPE: char = '\u{E100}';
const AUDIT_CONTROL_REPLACEMENT_BASE: u32 = 0xE120;
const FORBIDDEN_XML_CONTROLS: [u8; 29] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0B, 0x0C, 0x0E, 0x0F, 0x10, 0x11, 0x12,
    0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuditRecordCounts {
    pub elements: u64,
    pub empty_elements: u64,
    pub end_elements: u64,
    pub attributes: u64,
    pub element_texts: u64,
    pub tail_texts: u64,
    pub control_characters: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditSummary {
    pub record_schema: &'static str,
    pub record_sha256: String,
    pub record_count: u64,
    pub counts: AuditRecordCounts,
    pub entries: u64,
    pub headword_count: u64,
    pub headword_sha256: String,
    pub first_headword: String,
    pub last_headword: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuditMetadata {
    pub identifier: String,
    pub title: String,
    pub language: String,
    pub source: String,
    pub modified: String,
    pub collection: String,
    pub group_position: String,
    pub spine_documents: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditCheck {
    pub name: &'static str,
    pub passed: bool,
    pub expected: serde_json::Value,
    pub actual: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub dictionary: &'static str,
    pub volume: usize,
    pub volumes: usize,
    pub source: String,
    pub output: String,
    pub report: String,
    pub source_summary: AuditSummary,
    pub epub_summary: AuditSummary,
    pub metadata: AuditMetadata,
    pub checks: Vec<AuditCheck>,
    pub reproduction: String,
}

impl fmt::Display for AuditReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "status={}", self.status)?;
        writeln!(formatter, "dictionary={}", self.dictionary)?;
        writeln!(formatter, "volume={}", self.volume)?;
        writeln!(formatter, "volumes={}", self.volumes)?;
        writeln!(formatter, "source={}", self.source)?;
        writeln!(formatter, "output={}", self.output)?;
        writeln!(formatter, "report={}", self.report)?;
        writeln!(formatter, "entries={}", self.epub_summary.entries)?;
        writeln!(
            formatter,
            "record_schema={}",
            self.epub_summary.record_schema
        )?;
        write!(
            formatter,
            "record_sha256={}",
            self.epub_summary.record_sha256
        )
    }
}

#[derive(Debug)]
pub enum AuditError {
    Io(io::Error),
    Xml(quick_xml::Error),
    Encoding(EncodingError),
    InvalidUtf8(str::Utf8Error),
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
    Invalid(String),
    Mismatch {
        report: PathBuf,
        checks: Vec<&'static str>,
    },
}

impl AuditError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Mismatch { .. } => "KDEP-E011",
            Self::Io(_)
            | Self::Xml(_)
            | Self::Encoding(_)
            | Self::InvalidUtf8(_)
            | Self::Zip(_)
            | Self::Json(_)
            | Self::Invalid(_) => "KDEP-E010",
        }
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "audit I/O error: {error}"),
            Self::Xml(error) => write!(formatter, "audit XML parse error: {error}"),
            Self::Encoding(error) => write!(formatter, "audit XML decoding error: {error}"),
            Self::InvalidUtf8(error) => write!(formatter, "audit input is not UTF-8: {error}"),
            Self::Zip(error) => write!(formatter, "audit EPUB ZIP error: {error}"),
            Self::Json(error) => write!(formatter, "could not serialize audit report: {error}"),
            Self::Invalid(reason) => write!(formatter, "invalid audit input: {reason}"),
            Self::Mismatch { report, checks } => write!(
                formatter,
                "independent audit failed checks [{}]; report: {}",
                checks.join(", "),
                report.display()
            ),
        }
    }
}

impl Error for AuditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Xml(error) => Some(error),
            Self::Encoding(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::Zip(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Invalid(_) | Self::Mismatch { .. } => None,
        }
    }
}

impl From<io::Error> for AuditError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<quick_xml::Error> for AuditError {
    fn from(error: quick_xml::Error) -> Self {
        Self::Xml(error)
    }
}

impl From<EncodingError> for AuditError {
    fn from(error: EncodingError) -> Self {
        Self::Encoding(error)
    }
}

impl From<str::Utf8Error> for AuditError {
    fn from(error: str::Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}

impl From<zip::result::ZipError> for AuditError {
    fn from(error: zip::result::ZipError) -> Self {
        Self::Zip(error)
    }
}

impl From<serde_json::Error> for AuditError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn audit_volume(volume: &Volume, output_directory: &Path) -> Result<AuditReport, AuditError> {
    let output_path = output_directory.join(&volume.output_filename);
    let report_path = output_directory.join(format!("{}.audit.json", volume.output_filename));
    let source_summary = audit_source(&volume.source, volume.dictionary)?;
    let output_audit = audit_epub(&output_path)?;
    let expected_title = format!(
        "{} {:03}/{:03}",
        volume.dictionary.series(),
        volume.number,
        volume.total
    );
    let expected_source = volume.relative_source.to_string_lossy().replace('\\', "/");
    let expected_identifier = format!(
        "urn:uuid:{}",
        Uuid::new_v5(
            &BOOK_NAMESPACE,
            format!("{}/{}", volume.dictionary.key(), expected_source).as_bytes()
        )
    );

    let checks = vec![
        check(
            "record_sha256",
            &source_summary.record_sha256,
            &output_audit.summary.record_sha256,
        ),
        check(
            "record_counts",
            &source_summary.counts,
            &output_audit.summary.counts,
        ),
        check(
            "entries",
            &source_summary.entries,
            &output_audit.summary.entries,
        ),
        check(
            "headword_count",
            &source_summary.headword_count,
            &output_audit.summary.headword_count,
        ),
        check(
            "headword_sha256",
            &source_summary.headword_sha256,
            &output_audit.summary.headword_sha256,
        ),
        check(
            "first_headword",
            &source_summary.first_headword,
            &output_audit.summary.first_headword,
        ),
        check(
            "last_headword",
            &source_summary.last_headword,
            &output_audit.summary.last_headword,
        ),
        check(
            "output_filename",
            &volume.output_filename,
            &output_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        check(
            "metadata_title",
            &expected_title,
            &output_audit.metadata.title,
        ),
        check(
            "metadata_identifier",
            &expected_identifier,
            &output_audit.metadata.identifier,
        ),
        check(
            "metadata_language",
            &"ko",
            &output_audit.metadata.language.as_str(),
        ),
        check(
            "metadata_source",
            &expected_source,
            &output_audit.metadata.source,
        ),
        check(
            "metadata_modified",
            &MODIFIED_TIMESTAMP,
            &output_audit.metadata.modified.as_str(),
        ),
        check(
            "metadata_collection",
            &volume.dictionary.series(),
            &output_audit.metadata.collection.as_str(),
        ),
        check(
            "metadata_group_position",
            &volume.number.to_string(),
            &output_audit.metadata.group_position,
        ),
    ];

    let failed: Vec<&'static str> = checks
        .iter()
        .filter(|item| !item.passed)
        .map(|item| item.name)
        .collect();
    let status = if failed.is_empty() {
        "passed"
    } else {
        "failed"
    };
    let report = AuditReport {
        schema: AUDIT_SCHEMA,
        status,
        dictionary: volume.dictionary.key(),
        volume: volume.number,
        volumes: volume.total,
        source: expected_source,
        output: volume.output_filename.clone(),
        report: report_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        source_summary,
        epub_summary: output_audit.summary,
        metadata: output_audit.metadata,
        checks,
        reproduction: format!(
            "korean-dict-epub audit --dictionary {} --volume {}",
            volume.dictionary.key(),
            volume.number
        ),
    };
    write_report(&report_path, &report)?;

    if failed.is_empty() {
        Ok(report)
    } else {
        Err(AuditError::Mismatch {
            report: report_path,
            checks: failed,
        })
    }
}

fn check<T>(name: &'static str, expected: &T, actual: &T) -> AuditCheck
where
    T: PartialEq + Serialize,
{
    AuditCheck {
        name,
        passed: expected == actual,
        expected: json_value(expected),
        actual: json_value(actual),
    }
}

fn json_value<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::String(
        "<serialization failed>".to_owned(),
    ))
}

fn write_report(path: &Path, report: &AuditReport) -> Result<(), AuditError> {
    let mut output = AtomicWriteFile::options().open(path)?;
    serde_json::to_writer_pretty(&mut output, report)?;
    output.write_all(b"\n")?;
    output.commit()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditRecord {
    Start {
        depth: usize,
        name: String,
        attributes: Vec<(String, String)>,
    },
    Empty {
        depth: usize,
        name: String,
        attributes: Vec<(String, String)>,
    },
    Text {
        depth: usize,
        value: String,
    },
    Tail {
        depth: usize,
        value: String,
    },
    End {
        depth: usize,
        name: String,
    },
}

struct AuditDigest {
    hash: Sha256,
    counts: AuditRecordCounts,
}

impl AuditDigest {
    fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(DIGEST_PREAMBLE);
        Self {
            hash,
            counts: AuditRecordCounts::default(),
        }
    }

    fn update(&mut self, record: &AuditRecord) {
        match record {
            AuditRecord::Start {
                depth,
                name,
                attributes,
            } => {
                self.hash.update([0x01]);
                self.write_usize(*depth);
                self.write_string(name);
                self.write_attributes(attributes);
                self.counts.elements += 1;
                self.count_record_values(name, attributes);
            }
            AuditRecord::Empty {
                depth,
                name,
                attributes,
            } => {
                self.hash.update([0x02]);
                self.write_usize(*depth);
                self.write_string(name);
                self.write_attributes(attributes);
                self.counts.elements += 1;
                self.counts.empty_elements += 1;
                self.count_record_values(name, attributes);
            }
            AuditRecord::Text { depth, value } => {
                self.hash.update([0x03]);
                self.write_usize(*depth);
                self.write_string(value);
                self.counts.element_texts += 1;
                self.count_controls(value);
            }
            AuditRecord::Tail { depth, value } => {
                self.hash.update([0x04]);
                self.write_usize(*depth);
                self.write_string(value);
                self.counts.tail_texts += 1;
                self.count_controls(value);
            }
            AuditRecord::End { depth, name } => {
                self.hash.update([0x05]);
                self.write_usize(*depth);
                self.write_string(name);
                self.counts.end_elements += 1;
                self.count_controls(name);
            }
        }
    }

    fn finish(self) -> (String, AuditRecordCounts) {
        (hex_digest(self.hash.finalize().as_slice()), self.counts)
    }

    fn write_attributes(&mut self, attributes: &[(String, String)]) {
        self.write_usize(attributes.len());
        for (name, value) in attributes {
            self.write_string(name);
            self.write_string(value);
        }
        self.counts.attributes += attributes.len() as u64;
    }

    fn write_usize(&mut self, value: usize) {
        self.hash.update((value as u64).to_be_bytes());
    }

    fn write_string(&mut self, value: &str) {
        self.write_usize(value.len());
        self.hash.update(value.as_bytes());
    }

    fn count_record_values(&mut self, name: &str, attributes: &[(String, String)]) {
        self.count_controls(name);
        for (name, value) in attributes {
            self.count_controls(name);
            self.count_controls(value);
        }
    }

    fn count_controls(&mut self, value: &str) {
        self.counts.control_characters += value
            .chars()
            .filter(|character| is_forbidden_control(*character))
            .count() as u64;
    }
}

struct HeadwordDigest {
    hash: Sha256,
    count: u64,
    first: String,
    last: String,
}

impl HeadwordDigest {
    fn new() -> Self {
        Self {
            hash: Sha256::new(),
            count: 0,
            first: String::new(),
            last: String::new(),
        }
    }

    fn add(&mut self, value: &str) {
        let value = value.trim();
        if self.first.is_empty() {
            self.first = value.to_owned();
        }
        self.last = value.to_owned();
        self.count += 1;
        self.hash.update(value.as_bytes());
        self.hash.update(b"\n");
    }

    fn finish(self) -> (u64, String, String, String) {
        (
            self.count,
            hex_digest(self.hash.finalize().as_slice()),
            self.first,
            self.last,
        )
    }
}

struct SummaryAccumulator {
    dictionary: Dictionary,
    digest: AuditDigest,
    headwords: HeadwordDigest,
    stack: Vec<String>,
    entries: u64,
    entry_depth: Option<usize>,
    current_headword: Option<String>,
}

impl SummaryAccumulator {
    fn new(dictionary: Dictionary) -> Self {
        Self {
            dictionary,
            digest: AuditDigest::new(),
            headwords: HeadwordDigest::new(),
            stack: Vec::new(),
            entries: 0,
            entry_depth: None,
            current_headword: None,
        }
    }

    fn observe(&mut self, record: AuditRecord) {
        match &record {
            AuditRecord::Start {
                depth,
                name,
                attributes,
            } => {
                if local_name(name) == self.dictionary.entry_element() {
                    self.entries += 1;
                    self.entry_depth = Some(*depth);
                    self.current_headword = None;
                }
                self.capture_krdict_headword(name, attributes);
                self.stack.push(local_name(name).to_owned());
            }
            AuditRecord::Empty {
                depth: _,
                name,
                attributes,
            } => {
                if local_name(name) == self.dictionary.entry_element() {
                    self.entries += 1;
                    self.headwords.add(&format!("항목 {}", self.entries));
                } else {
                    self.capture_krdict_headword(name, attributes);
                }
            }
            AuditRecord::Text { value, .. } => {
                if self.current_headword.is_none()
                    && self.dictionary != Dictionary::Krdict
                    && self.stack.last().map(String::as_str) == Some("word")
                {
                    let expected_parent = if self.dictionary == Dictionary::Stdict {
                        "word_info"
                    } else {
                        "wordInfo"
                    };
                    if self.stack.iter().rev().nth(1).map(String::as_str) == Some(expected_parent)
                        && !value.trim().is_empty()
                    {
                        self.current_headword = Some(value.trim().to_owned());
                    }
                }
            }
            AuditRecord::Tail { .. } => {}
            AuditRecord::End { depth, name } => {
                if self.entry_depth == Some(*depth)
                    && local_name(name) == self.dictionary.entry_element()
                {
                    let value = self
                        .current_headword
                        .take()
                        .unwrap_or_else(|| format!("항목 {}", self.entries));
                    self.headwords.add(&value);
                    self.entry_depth = None;
                }
                self.stack.pop();
            }
        }
        self.digest.update(&record);
    }

    fn capture_krdict_headword(&mut self, name: &str, attributes: &[(String, String)]) {
        if self.dictionary != Dictionary::Krdict
            || self.entry_depth.is_none()
            || self.current_headword.is_some()
            || local_name(name) != "feat"
        {
            return;
        }
        let attribute = |candidate: &str| {
            attributes
                .iter()
                .find(|(name, _)| local_name(name) == candidate)
                .map(|(_, value)| value.as_str())
        };
        if attribute("att") == Some("writtenForm")
            && let Some(value) = attribute("val").filter(|value| !value.trim().is_empty())
        {
            self.current_headword = Some(value.trim().to_owned());
        }
    }

    fn finish(self) -> AuditSummary {
        let (record_sha256, counts) = self.digest.finish();
        let record_count =
            counts.elements + counts.end_elements + counts.element_texts + counts.tail_texts;
        let (headword_count, headword_sha256, first_headword, last_headword) =
            self.headwords.finish();
        AuditSummary {
            record_schema: DIGEST_SCHEMA,
            record_sha256,
            record_count,
            counts,
            entries: self.entries,
            headword_count,
            headword_sha256,
            first_headword,
            last_headword,
        }
    }
}

#[derive(Debug)]
struct AuditFrame {
    depth: usize,
    child_count: usize,
    text: String,
}

fn audit_source(path: &Path, dictionary: Dictionary) -> Result<AuditSummary, AuditError> {
    let input = File::open(path)?;
    let sanitized = AuditControlReader::new(input);
    let buffered = BufReader::with_capacity(XML_BUFFER_CAPACITY, sanitized);
    let mut reader = Reader::from_reader(buffered);
    reader.config_mut().enable_all_checks(true);
    let mut buffer = Vec::with_capacity(XML_BUFFER_CAPACITY);
    let mut frames: Vec<AuditFrame> = Vec::new();
    let mut summary = SummaryAccumulator::new(dictionary);

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer)? {
            Event::Start(element) => {
                flush_frame_text(&mut frames, &mut summary)?;
                if let Some(parent) = frames.last_mut() {
                    parent.child_count += 1;
                }
                let depth = frames.len();
                summary.observe(AuditRecord::Start {
                    depth,
                    name: audit_decode_name(element.name().as_ref())?,
                    attributes: audit_decode_attributes(&element)?,
                });
                frames.push(AuditFrame {
                    depth,
                    child_count: 0,
                    text: String::new(),
                });
            }
            Event::Empty(element) => {
                flush_frame_text(&mut frames, &mut summary)?;
                if let Some(parent) = frames.last_mut() {
                    parent.child_count += 1;
                }
                summary.observe(AuditRecord::Empty {
                    depth: frames.len(),
                    name: audit_decode_name(element.name().as_ref())?,
                    attributes: audit_decode_attributes(&element)?,
                });
            }
            Event::Text(text) => {
                append_source_text(
                    &mut frames,
                    &audit_restore_controls(text.xml10_content()?.as_ref())?,
                )?;
            }
            Event::CData(text) => {
                append_source_text(
                    &mut frames,
                    &audit_restore_controls(text.xml10_content()?.as_ref())?,
                )?;
            }
            Event::GeneralRef(reference) => {
                append_source_text(&mut frames, &audit_resolve_reference(&reference)?)?;
            }
            Event::End(element) => {
                flush_frame_text(&mut frames, &mut summary)?;
                let frame = frames
                    .pop()
                    .ok_or_else(|| AuditError::Invalid("unexpected closing element".to_owned()))?;
                summary.observe(AuditRecord::End {
                    depth: frame.depth,
                    name: audit_decode_name(element.name().as_ref())?,
                });
            }
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_) => {}
        }
    }
    if !frames.is_empty() {
        return Err(AuditError::Invalid(
            "source XML ended with open elements".to_owned(),
        ));
    }
    Ok(summary.finish())
}

fn flush_frame_text(
    frames: &mut [AuditFrame],
    summary: &mut SummaryAccumulator,
) -> Result<(), AuditError> {
    let Some(frame) = frames.last_mut() else {
        return Ok(());
    };
    if frame.text.is_empty() || frame.text.chars().all(char::is_whitespace) {
        frame.text.clear();
        return Ok(());
    }
    let value = std::mem::take(&mut frame.text);
    if frame.child_count == 0 {
        summary.observe(AuditRecord::Text {
            depth: frame.depth,
            value,
        });
    } else {
        summary.observe(AuditRecord::Tail {
            depth: frame.depth + 1,
            value,
        });
    }
    Ok(())
}

fn append_source_text(frames: &mut [AuditFrame], value: &str) -> Result<(), AuditError> {
    if let Some(frame) = frames.last_mut() {
        frame.text.push_str(value);
        return Ok(());
    }
    if value.chars().all(char::is_whitespace) {
        Ok(())
    } else {
        Err(AuditError::Invalid(format!(
            "meaningful source text outside root: {value:?}"
        )))
    }
}

fn audit_decode_name(bytes: &[u8]) -> Result<String, AuditError> {
    audit_restore_controls(str::from_utf8(bytes)?)
}

fn audit_decode_attributes(element: &BytesStart<'_>) -> Result<Vec<(String, String)>, AuditError> {
    let mut output = Vec::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        output.push((
            audit_decode_name(attribute.key.as_ref())?,
            audit_restore_controls(
                attribute
                    .normalized_value(XmlVersion::Implicit1_0)?
                    .as_ref(),
            )?,
        ));
    }
    Ok(output)
}

fn audit_resolve_reference(reference: &BytesRef<'_>) -> Result<String, AuditError> {
    if let Some(character) = reference.resolve_char_ref()? {
        return audit_restore_controls(&character.to_string());
    }
    let name = reference.xml10_content()?;
    let value = match name.as_ref() {
        "lt" => "<",
        "gt" => ">",
        "amp" => "&",
        "apos" => "'",
        "quot" => "\"",
        other => {
            return Err(AuditError::Invalid(format!(
                "unrecognized source entity: &{other};"
            )));
        }
    };
    Ok(value.to_owned())
}

fn audit_restore_controls(value: &str) -> Result<String, AuditError> {
    if !value.contains(AUDIT_CONTROL_ESCAPE) {
        return Ok(value.to_owned());
    }
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != AUDIT_CONTROL_ESCAPE {
            output.push(character);
            continue;
        }
        let escaped = characters
            .next()
            .ok_or_else(|| AuditError::Invalid("truncated audit control escape".to_owned()))?;
        if escaped == AUDIT_CONTROL_ESCAPE {
            output.push(AUDIT_CONTROL_ESCAPE);
            continue;
        }
        let codepoint = u32::from(escaped)
            .checked_sub(AUDIT_CONTROL_REPLACEMENT_BASE)
            .filter(|value| *value < 0x20)
            .ok_or_else(|| AuditError::Invalid("invalid audit control escape".to_owned()))?;
        output.push(
            char::from_u32(codepoint)
                .ok_or_else(|| AuditError::Invalid("invalid control scalar".to_owned()))?,
        );
    }
    Ok(output)
}

struct AuditControlReader<R: Read> {
    input: R,
    input_buffer: [u8; SANITIZER_INPUT_CAPACITY],
    output: Vec<u8>,
    position: usize,
    prefix_match: Vec<u8>,
    eof: bool,
}

impl<R: Read> AuditControlReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            input_buffer: [0; SANITIZER_INPUT_CAPACITY],
            output: Vec::with_capacity(SANITIZER_INPUT_CAPACITY * 2),
            position: 0,
            prefix_match: Vec::with_capacity(AUDIT_CONTROL_ESCAPE.len_utf8()),
            eof: false,
        }
    }

    fn refill(&mut self) -> io::Result<()> {
        self.output.clear();
        self.position = 0;
        let count = self.input.read(&mut self.input_buffer)?;
        if count == 0 {
            self.output.append(&mut self.prefix_match);
            self.eof = true;
            return Ok(());
        }
        let prefix = AUDIT_CONTROL_ESCAPE.to_string().into_bytes();
        for byte in &self.input_buffer[..count] {
            if *byte == prefix[self.prefix_match.len()] {
                self.prefix_match.push(*byte);
                if self.prefix_match.len() == prefix.len() {
                    self.output.extend_from_slice(&prefix);
                    self.output.extend_from_slice(&prefix);
                    self.prefix_match.clear();
                }
                continue;
            }
            self.output.append(&mut self.prefix_match);
            if *byte == prefix[0] {
                self.prefix_match.push(*byte);
            } else if FORBIDDEN_XML_CONTROLS.contains(byte) {
                self.output.extend_from_slice(&prefix);
                let replacement = char::from_u32(AUDIT_CONTROL_REPLACEMENT_BASE + u32::from(*byte))
                    .expect("audit replacement is a Unicode scalar");
                let mut encoded = [0; 4];
                self.output
                    .extend_from_slice(replacement.encode_utf8(&mut encoded).as_bytes());
            } else {
                self.output.push(*byte);
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for AuditControlReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.position == self.output.len() && !self.eof {
            self.refill()?;
        }
        if self.position == self.output.len() {
            return Ok(0);
        }
        let count = (self.output.len() - self.position).min(buffer.len());
        buffer[..count].copy_from_slice(&self.output[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

struct EpubAudit {
    summary: AuditSummary,
    metadata: AuditMetadata,
}

struct PackageInfo {
    metadata: AuditMetadata,
    documents: Vec<String>,
}

fn audit_epub(path: &Path) -> Result<EpubAudit, AuditError> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let package_bytes = read_zip_member(&mut archive, "EPUB/package.opf")?;
    let package = parse_package(&package_bytes)?;
    let mut digest = AuditDigest::new();
    let mut headwords = HeadwordDigest::new();
    let mut entries = 0_u64;

    for document in &package.documents {
        let bytes = read_zip_member(&mut archive, document)?;
        audit_xhtml(&bytes, &mut digest, &mut headwords, &mut entries)?;
    }
    let (record_sha256, counts) = digest.finish();
    let record_count =
        counts.elements + counts.end_elements + counts.element_texts + counts.tail_texts;
    let (headword_count, headword_sha256, first_headword, last_headword) = headwords.finish();

    Ok(EpubAudit {
        summary: AuditSummary {
            record_schema: DIGEST_SCHEMA,
            record_sha256,
            record_count,
            counts,
            entries,
            headword_count,
            headword_sha256,
            first_headword,
            last_headword,
        },
        metadata: package.metadata,
    })
}

fn read_zip_member<R: Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, AuditError> {
    let mut member = archive.by_name(name)?;
    let mut bytes = Vec::new();
    member.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum MetadataField {
    Identifier,
    Title,
    Language,
    Source,
    Modified,
    Collection,
    GroupPosition,
}

fn parse_package(bytes: &[u8]) -> Result<PackageInfo, AuditError> {
    let mut reader = Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().enable_all_checks(true);
    let mut buffer = Vec::new();
    let mut metadata = AuditMetadata::default();
    let mut manifest: HashMap<String, String> = HashMap::new();
    let mut spine = Vec::new();
    let mut field: Option<MetadataField> = None;

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer)? {
            Event::Start(element) | Event::Empty(element) => {
                let empty = element.is_empty();
                let name = str::from_utf8(element.name().as_ref())?.to_owned();
                let attributes = element_attributes(&element)?;
                match name.as_str() {
                    "dc:identifier" => field = Some(MetadataField::Identifier),
                    "dc:title" => field = Some(MetadataField::Title),
                    "dc:language" => field = Some(MetadataField::Language),
                    "dc:source" => field = Some(MetadataField::Source),
                    "meta" => match attribute(&attributes, "property") {
                        Some("dcterms:modified") => field = Some(MetadataField::Modified),
                        Some("belongs-to-collection") => field = Some(MetadataField::Collection),
                        Some("group-position") => field = Some(MetadataField::GroupPosition),
                        _ => {}
                    },
                    "item" => {
                        if let (Some(id), Some(href), Some(media_type)) = (
                            attribute(&attributes, "id"),
                            attribute(&attributes, "href"),
                            attribute(&attributes, "media-type"),
                        ) && media_type == "application/xhtml+xml"
                        {
                            manifest.insert(id.to_owned(), safe_member_path(href)?);
                        }
                    }
                    "itemref" => {
                        if let Some(idref) = attribute(&attributes, "idref") {
                            spine.push(idref.to_owned());
                        }
                    }
                    _ => {}
                }
                if empty {
                    field = None;
                }
            }
            Event::Text(text) => {
                append_metadata(&mut metadata, field, text.xml10_content()?.as_ref());
            }
            Event::GeneralRef(reference) => {
                append_metadata(&mut metadata, field, &xhtml_reference(&reference)?);
            }
            Event::End(element) => {
                let name = str::from_utf8(element.name().as_ref())?.to_owned();
                if matches!(
                    name.as_str(),
                    "dc:identifier" | "dc:title" | "dc:language" | "dc:source" | "meta"
                ) {
                    field = None;
                }
            }
            Event::Eof => break,
            Event::Decl(_)
            | Event::Comment(_)
            | Event::PI(_)
            | Event::DocType(_)
            | Event::CData(_) => {}
        }
    }

    let mut documents = Vec::with_capacity(spine.len());
    for idref in spine {
        let href = manifest
            .get(&idref)
            .ok_or_else(|| AuditError::Invalid(format!("unknown spine idref: {idref}")))?;
        documents.push(href.clone());
    }
    if documents.is_empty() {
        return Err(AuditError::Invalid(
            "EPUB package has no XHTML spine documents".to_owned(),
        ));
    }
    metadata.spine_documents = documents.len();
    Ok(PackageInfo {
        metadata,
        documents,
    })
}

fn append_metadata(metadata: &mut AuditMetadata, field: Option<MetadataField>, value: &str) {
    match field {
        Some(MetadataField::Identifier) => metadata.identifier.push_str(value),
        Some(MetadataField::Title) => metadata.title.push_str(value),
        Some(MetadataField::Language) => metadata.language.push_str(value),
        Some(MetadataField::Source) => metadata.source.push_str(value),
        Some(MetadataField::Modified) => metadata.modified.push_str(value),
        Some(MetadataField::Collection) => metadata.collection.push_str(value),
        Some(MetadataField::GroupPosition) => metadata.group_position.push_str(value),
        None => {}
    }
}

fn safe_member_path(href: &str) -> Result<String, AuditError> {
    let path = Path::new(href);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || href.contains('\\')
    {
        return Err(AuditError::Invalid(format!(
            "unsafe EPUB manifest href: {href}"
        )));
    }
    Ok(format!("EPUB/{href}"))
}

#[derive(Clone, Copy)]
enum RecordTarget {
    Name,
    AttributeName,
    AttributeValue,
    Value,
}

#[derive(Default)]
struct ElementContext {
    record_root: bool,
    attribute_root: bool,
    target: Option<RecordTarget>,
    suppress_record_text: bool,
    heading_root: bool,
    suppress_heading_text: bool,
}

struct RecordCapture {
    kind: String,
    depth: usize,
    name: String,
    attributes: Vec<(String, String)>,
    current_attribute: Option<(String, String)>,
    value: String,
}

fn audit_xhtml(
    bytes: &[u8],
    digest: &mut AuditDigest,
    headwords: &mut HeadwordDigest,
    entries: &mut u64,
) -> Result<(), AuditError> {
    let mut reader = Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().enable_all_checks(true);
    let mut buffer = Vec::new();
    let mut stack: Vec<ElementContext> = Vec::new();
    let mut record: Option<RecordCapture> = None;
    let mut heading: Option<String> = None;

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer)? {
            Event::Start(element) => {
                let context =
                    begin_xhtml_element(&element, &mut record, &mut heading, entries, &stack)?;
                stack.push(context);
            }
            Event::Empty(element) => {
                let context =
                    begin_xhtml_element(&element, &mut record, &mut heading, entries, &stack)?;
                finish_xhtml_element(context, &mut record, &mut heading, digest, headwords)?;
            }
            Event::Text(text) => {
                append_xhtml_text(
                    text.xml10_content()?.as_ref(),
                    &stack,
                    &mut record,
                    &mut heading,
                )?;
            }
            Event::GeneralRef(reference) => {
                append_xhtml_text(
                    &xhtml_reference(&reference)?,
                    &stack,
                    &mut record,
                    &mut heading,
                )?;
            }
            Event::End(_) => {
                let context = stack.pop().ok_or_else(|| {
                    AuditError::Invalid("XHTML ended an element without a start".to_owned())
                })?;
                finish_xhtml_element(context, &mut record, &mut heading, digest, headwords)?;
            }
            Event::Eof => break,
            Event::Decl(_)
            | Event::Comment(_)
            | Event::PI(_)
            | Event::DocType(_)
            | Event::CData(_) => {}
        }
    }
    if record.is_some() || heading.is_some() || !stack.is_empty() {
        return Err(AuditError::Invalid(
            "XHTML ended with incomplete audit markup".to_owned(),
        ));
    }
    Ok(())
}

fn begin_xhtml_element(
    element: &BytesStart<'_>,
    record: &mut Option<RecordCapture>,
    heading: &mut Option<String>,
    entries: &mut u64,
    stack: &[ElementContext],
) -> Result<ElementContext, AuditError> {
    let name = str::from_utf8(element.name().as_ref())?.to_owned();
    let local = local_name(&name);
    let attributes = element_attributes(element)?;
    let classes = attribute(&attributes, "class").unwrap_or_default();
    let has_class = |candidate: &str| classes.split_whitespace().any(|class| class == candidate);
    let mut context = ElementContext::default();

    if local == "article" && has_class("entry") {
        *entries += 1;
    }
    if heading.is_none() && local == "h2" && has_class("entry-heading") {
        *heading = Some(String::new());
        context.heading_root = true;
    }

    if record.is_none() && attribute(&attributes, "data-kdep-record") == Some("true") {
        *record = Some(RecordCapture {
            kind: required_attribute(&attributes, "data-kdep-kind")?.to_owned(),
            depth: required_attribute(&attributes, "data-kdep-depth")?
                .parse()
                .map_err(|_| AuditError::Invalid("invalid record depth".to_owned()))?,
            name: String::new(),
            attributes: Vec::new(),
            current_attribute: None,
            value: String::new(),
        });
        context.record_root = true;
    } else if record.is_some() {
        if has_class("xml-attribute") {
            let capture = record.as_mut().expect("record exists");
            if capture.current_attribute.is_some() {
                return Err(AuditError::Invalid(
                    "nested XHTML attribute audit markup".to_owned(),
                ));
            }
            capture.current_attribute = Some((String::new(), String::new()));
            context.attribute_root = true;
        }
        context.target = if has_class("xml-name") {
            Some(RecordTarget::Name)
        } else if has_class("xml-attribute-name") {
            Some(RecordTarget::AttributeName)
        } else if has_class("xml-attribute-value") {
            Some(RecordTarget::AttributeValue)
        } else if has_class("xml-text-value") || has_class("xml-tail-value") {
            Some(RecordTarget::Value)
        } else {
            None
        };
    }

    if has_class("xml-control") {
        let codepoint = parse_control_marker(required_attribute(&attributes, "data-codepoint")?)?;
        append_control_to_record(codepoint, stack, &context, record)?;
        if heading.is_some() {
            heading.as_mut().expect("heading exists").push(codepoint);
        }
        context.suppress_record_text = true;
        context.suppress_heading_text = true;
    }
    Ok(context)
}

fn finish_xhtml_element(
    context: ElementContext,
    record: &mut Option<RecordCapture>,
    heading: &mut Option<String>,
    digest: &mut AuditDigest,
    headwords: &mut HeadwordDigest,
) -> Result<(), AuditError> {
    if context.attribute_root {
        let capture = record.as_mut().ok_or_else(|| {
            AuditError::Invalid("attribute markup ended outside a record".to_owned())
        })?;
        let attribute = capture.current_attribute.take().ok_or_else(|| {
            AuditError::Invalid("attribute markup has no captured value".to_owned())
        })?;
        if attribute.0.is_empty() {
            return Err(AuditError::Invalid(
                "attribute markup is missing its name".to_owned(),
            ));
        }
        capture.attributes.push(attribute);
    }
    if context.record_root {
        let capture = record
            .take()
            .ok_or_else(|| AuditError::Invalid("record markup ended twice".to_owned()))?;
        digest.update(&capture.finish()?);
    }
    if context.heading_root {
        let value = heading
            .take()
            .ok_or_else(|| AuditError::Invalid("heading markup ended twice".to_owned()))?;
        headwords.add(&value);
    }
    Ok(())
}

impl RecordCapture {
    fn finish(self) -> Result<AuditRecord, AuditError> {
        if self.current_attribute.is_some() {
            return Err(AuditError::Invalid(
                "record ended with an incomplete attribute".to_owned(),
            ));
        }
        match self.kind.as_str() {
            "start" if !self.name.is_empty() => Ok(AuditRecord::Start {
                depth: self.depth,
                name: self.name,
                attributes: self.attributes,
            }),
            "empty" if !self.name.is_empty() => Ok(AuditRecord::Empty {
                depth: self.depth,
                name: self.name,
                attributes: self.attributes,
            }),
            "text" => Ok(AuditRecord::Text {
                depth: self.depth,
                value: self.value,
            }),
            "tail" => Ok(AuditRecord::Tail {
                depth: self.depth,
                value: self.value,
            }),
            "end" if !self.name.is_empty() => Ok(AuditRecord::End {
                depth: self.depth,
                name: self.name,
            }),
            "start" | "empty" | "end" => Err(AuditError::Invalid(format!(
                "{} record is missing its QName",
                self.kind
            ))),
            _ => Err(AuditError::Invalid(format!(
                "unknown XHTML record kind: {}",
                self.kind
            ))),
        }
    }
}

fn append_xhtml_text(
    value: &str,
    stack: &[ElementContext],
    record: &mut Option<RecordCapture>,
    heading: &mut Option<String>,
) -> Result<(), AuditError> {
    if record.is_some()
        && !stack
            .iter()
            .rev()
            .any(|context| context.suppress_record_text)
        && let Some(target) = stack.iter().rev().find_map(|context| context.target)
    {
        append_record_target(record, target, value)?;
    }
    if let Some(heading) = heading
        && !stack
            .iter()
            .rev()
            .any(|context| context.suppress_heading_text)
    {
        heading.push_str(value);
    }
    Ok(())
}

fn append_control_to_record(
    value: char,
    stack: &[ElementContext],
    context: &ElementContext,
    record: &mut Option<RecordCapture>,
) -> Result<(), AuditError> {
    let target = context
        .target
        .or_else(|| stack.iter().rev().find_map(|context| context.target));
    if let Some(target) = target {
        let mut encoded = [0; 4];
        append_record_target(record, target, value.encode_utf8(&mut encoded))?;
    }
    Ok(())
}

fn append_record_target(
    record: &mut Option<RecordCapture>,
    target: RecordTarget,
    value: &str,
) -> Result<(), AuditError> {
    let capture = record
        .as_mut()
        .ok_or_else(|| AuditError::Invalid("record text appeared outside a record".to_owned()))?;
    match target {
        RecordTarget::Name => capture.name.push_str(value),
        RecordTarget::AttributeName => capture
            .current_attribute
            .as_mut()
            .ok_or_else(|| {
                AuditError::Invalid("attribute name appeared outside an attribute".to_owned())
            })?
            .0
            .push_str(value),
        RecordTarget::AttributeValue => capture
            .current_attribute
            .as_mut()
            .ok_or_else(|| {
                AuditError::Invalid("attribute value appeared outside an attribute".to_owned())
            })?
            .1
            .push_str(value),
        RecordTarget::Value => capture.value.push_str(value),
    }
    Ok(())
}

fn parse_control_marker(value: &str) -> Result<char, AuditError> {
    let digits = value
        .strip_prefix("U+")
        .filter(|digits| (4..=6).contains(&digits.len()))
        .ok_or_else(|| AuditError::Invalid(format!("invalid control marker: {value}")))?;
    let codepoint = u32::from_str_radix(digits, 16)
        .map_err(|_| AuditError::Invalid(format!("invalid control marker: {value}")))?;
    let character = char::from_u32(codepoint)
        .ok_or_else(|| AuditError::Invalid(format!("invalid control marker: {value}")))?;
    if !is_forbidden_control(character) {
        return Err(AuditError::Invalid(format!(
            "marker is not an XML-forbidden control: {value}"
        )));
    }
    Ok(character)
}

fn element_attributes(element: &BytesStart<'_>) -> Result<Vec<(String, String)>, AuditError> {
    let mut attributes = Vec::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        attributes.push((
            str::from_utf8(attribute.key.as_ref())?.to_owned(),
            attribute
                .normalized_value(XmlVersion::Implicit1_0)?
                .into_owned(),
        ));
    }
    Ok(attributes)
}

fn attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_str())
}

fn required_attribute<'a>(
    attributes: &'a [(String, String)],
    name: &str,
) -> Result<&'a str, AuditError> {
    attribute(attributes, name)
        .ok_or_else(|| AuditError::Invalid(format!("missing XHTML attribute: {name}")))
}

fn xhtml_reference(reference: &BytesRef<'_>) -> Result<String, AuditError> {
    if let Some(character) = reference.resolve_char_ref()? {
        return Ok(character.to_string());
    }
    let name = reference.xml10_content()?;
    let value = match name.as_ref() {
        "lt" => "<",
        "gt" => ">",
        "amp" => "&",
        "apos" => "'",
        "quot" => "\"",
        other => {
            return Err(AuditError::Invalid(format!(
                "unrecognized XHTML entity: &{other};"
            )));
        }
    };
    Ok(value.to_owned())
}

fn is_forbidden_control(character: char) -> bool {
    let codepoint = u32::from(character);
    codepoint < 0x20 && !matches!(codepoint, 0x09 | 0x0A | 0x0D)
}

fn local_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit_once(':')
        .map_or(qualified_name, |(_, local)| local)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::{AuditControlReader, audit_restore_controls};

    #[test]
    fn independent_control_escape_round_trips_real_marker_and_raw_control() {
        let marker = super::AUDIT_CONTROL_ESCAPE.to_string();
        let mut input = format!("<root>{marker}앞").into_bytes();
        input.push(0x08);
        input.extend_from_slice(format!("뒤{marker}</root>").as_bytes());
        let mut sanitized = String::new();
        AuditControlReader::new(Cursor::new(input))
            .read_to_string(&mut sanitized)
            .expect("sanitizer should stream");

        assert!(!sanitized.contains('\u{0008}'));
        let text = sanitized
            .strip_prefix("<root>")
            .and_then(|value| value.strip_suffix("</root>"))
            .expect("fixture wrapper should remain");
        assert_eq!(
            audit_restore_controls(text).expect("controls should restore"),
            format!("{marker}앞\u{0008}뒤{marker}")
        );
    }
}
