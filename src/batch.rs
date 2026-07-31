use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use atomic_write_file::AtomicWriteFile;
use serde::Serialize;

use crate::audit::{AuditError, AuditReport, audit_volume};
use crate::catalog::Volume;
use crate::epub::{BuildOptions, BuildReport, EpubError, build_volume};

const REPORT_SCHEMA: &str = "kdep-corpus-report-v1";
const REPORT_FILENAME: &str = "corpus-report.json";

#[derive(Debug, Clone)]
pub struct EpubCheckOptions {
    pub java: PathBuf,
    pub jar: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BatchOptions {
    pub jobs: usize,
    pub overwrite: bool,
    pub resume: bool,
    pub keep_going: bool,
    pub build: BuildOptions,
    pub epubcheck: Option<EpubCheckOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorpusBookReport {
    pub dictionary: &'static str,
    pub volume: usize,
    pub volumes: usize,
    pub source: String,
    pub output: String,
    pub status: &'static str,
    pub built: bool,
    pub resumed: bool,
    pub entries: Option<u64>,
    pub chapters: Option<usize>,
    pub first_headword: Option<String>,
    pub last_headword: Option<String>,
    pub record_sha256: Option<String>,
    pub audit: &'static str,
    pub epubcheck: &'static str,
    pub epubcheck_log: String,
    pub error_code: Option<&'static str>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorpusReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub report: &'static str,
    pub expected_volumes: usize,
    pub processed_volumes: usize,
    pub passed_volumes: usize,
    pub failed_volumes: usize,
    pub unprocessed_volumes: usize,
    pub total_entries: u64,
    pub jobs: usize,
    pub overwrite: bool,
    pub resume: bool,
    pub keep_going: bool,
    pub entries_per_chapter: usize,
    pub chapter_bytes: usize,
    pub epubcheck_enabled: bool,
    pub epubcheck_jar: Option<String>,
    pub books: Vec<CorpusBookReport>,
}

impl fmt::Display for CorpusReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "status={}", self.status)?;
        writeln!(formatter, "report={}", self.report)?;
        writeln!(formatter, "expected_volumes={}", self.expected_volumes)?;
        writeln!(formatter, "processed_volumes={}", self.processed_volumes)?;
        writeln!(formatter, "passed_volumes={}", self.passed_volumes)?;
        writeln!(formatter, "failed_volumes={}", self.failed_volumes)?;
        writeln!(
            formatter,
            "unprocessed_volumes={}",
            self.unprocessed_volumes
        )?;
        writeln!(formatter, "total_entries={}", self.total_entries)?;
        write!(formatter, "epubcheck_enabled={}", self.epubcheck_enabled)
    }
}

#[derive(Debug)]
pub enum BatchError {
    InvalidOptions(&'static str),
    ExistingOutputs(Vec<PathBuf>),
    InvalidEpubCheckJar(PathBuf),
    Io(io::Error),
    Json(serde_json::Error),
    VolumeFailures {
        report: PathBuf,
        failed: usize,
        unprocessed: usize,
    },
}

impl BatchError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::VolumeFailures { .. } => "KDEP-E013",
            Self::InvalidOptions(_)
            | Self::ExistingOutputs(_)
            | Self::InvalidEpubCheckJar(_)
            | Self::Io(_)
            | Self::Json(_) => "KDEP-E012",
        }
    }
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions(reason) => write!(formatter, "invalid batch options: {reason}"),
            Self::ExistingOutputs(paths) => {
                let names: Vec<_> = paths
                    .iter()
                    .take(5)
                    .map(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string())
                    })
                    .collect();
                write!(
                    formatter,
                    "{} output EPUB(s) already exist [{}]; pass --resume or --overwrite",
                    paths.len(),
                    names.join(", ")
                )
            }
            Self::InvalidEpubCheckJar(path) => write!(
                formatter,
                "EPUBCheck jar is missing or is not a file: {}",
                path.display()
            ),
            Self::Io(error) => write!(formatter, "batch I/O error: {error}"),
            Self::Json(error) => write!(formatter, "could not serialize corpus report: {error}"),
            Self::VolumeFailures {
                report,
                failed,
                unprocessed,
            } => write!(
                formatter,
                "batch failed for {failed} volume(s), {unprocessed} unprocessed; report: {}",
                report.display()
            ),
        }
    }
}

impl Error for BatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidOptions(_)
            | Self::ExistingOutputs(_)
            | Self::InvalidEpubCheckJar(_)
            | Self::VolumeFailures { .. } => None,
        }
    }
}

impl From<io::Error> for BatchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for BatchError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn run_batch(
    volumes: Vec<Volume>,
    output_directory: &Path,
    options: BatchOptions,
) -> Result<CorpusReport, BatchError> {
    validate_options(&volumes, output_directory, &options)?;
    fs::create_dir_all(output_directory)?;

    let expected_volumes = volumes.len();
    let queue = Arc::new(Mutex::new(VecDeque::from(volumes)));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();

    thread::scope(|scope| {
        for _ in 0..options.jobs.min(expected_volumes) {
            let queue = Arc::clone(&queue);
            let cancelled = Arc::clone(&cancelled);
            let sender = sender.clone();
            let options = options.clone();
            scope.spawn(move || {
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let volume = {
                        let mut queue = queue.lock().expect("batch queue lock should not poison");
                        queue.pop_front()
                    };
                    let Some(volume) = volume else {
                        break;
                    };
                    let report = process_volume(&volume, output_directory, &options);
                    let failed = report.status == "failed";
                    if sender.send(report).is_err() {
                        break;
                    }
                    if failed && !options.keep_going {
                        cancelled.store(true, Ordering::Release);
                    }
                }
            });
        }
        drop(sender);
    });

    let mut books: Vec<_> = receiver.into_iter().collect();
    books.sort_by_key(|book| (dictionary_order(book.dictionary), book.volume));
    let processed_volumes = books.len();
    let failed_volumes = books.iter().filter(|book| book.status == "failed").count();
    let passed_volumes = books.iter().filter(|book| book.status == "passed").count();
    let unprocessed_volumes = expected_volumes.saturating_sub(processed_volumes);
    let total_entries = books.iter().filter_map(|book| book.entries).sum();
    let status = if failed_volumes > 0 || unprocessed_volumes > 0 {
        "failed"
    } else if options.epubcheck.is_some() {
        "passed"
    } else {
        "partial"
    };
    let report = CorpusReport {
        schema: REPORT_SCHEMA,
        status,
        report: REPORT_FILENAME,
        expected_volumes,
        processed_volumes,
        passed_volumes,
        failed_volumes,
        unprocessed_volumes,
        total_entries,
        jobs: options.jobs,
        overwrite: options.overwrite,
        resume: options.resume,
        keep_going: options.keep_going,
        entries_per_chapter: options.build.entries_per_chapter,
        chapter_bytes: options.build.chapter_bytes,
        epubcheck_enabled: options.epubcheck.is_some(),
        epubcheck_jar: options.epubcheck.as_ref().map(|configuration| {
            configuration
                .jar
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| configuration.jar.display().to_string())
        }),
        books,
    };
    let report_path = output_directory.join(REPORT_FILENAME);
    write_report(&report_path, &report)?;

    if status == "failed" {
        Err(BatchError::VolumeFailures {
            report: report_path,
            failed: failed_volumes,
            unprocessed: unprocessed_volumes,
        })
    } else {
        Ok(report)
    }
}

fn validate_options(
    volumes: &[Volume],
    output_directory: &Path,
    options: &BatchOptions,
) -> Result<(), BatchError> {
    if volumes.is_empty() {
        return Err(BatchError::InvalidOptions(
            "at least one tracked volume is required",
        ));
    }
    if options.jobs == 0 {
        return Err(BatchError::InvalidOptions(
            "worker count must be at least one",
        ));
    }
    if options.overwrite && options.resume {
        return Err(BatchError::InvalidOptions(
            "--overwrite and --resume are mutually exclusive",
        ));
    }
    if let Some(configuration) = &options.epubcheck
        && !configuration.jar.is_file()
    {
        return Err(BatchError::InvalidEpubCheckJar(configuration.jar.clone()));
    }
    if !options.overwrite && !options.resume {
        let existing: Vec<_> = volumes
            .iter()
            .map(|volume| output_directory.join(&volume.output_filename))
            .filter(|path| path.exists())
            .collect();
        if !existing.is_empty() {
            return Err(BatchError::ExistingOutputs(existing));
        }
    }
    Ok(())
}

fn process_volume(
    volume: &Volume,
    output_directory: &Path,
    options: &BatchOptions,
) -> CorpusBookReport {
    let output_path = output_directory.join(&volume.output_filename);
    let resumed = options.resume && output_path.is_file();
    let mut report = CorpusBookReport {
        dictionary: volume.dictionary.key(),
        volume: volume.number,
        volumes: volume.total,
        source: volume.relative_source.to_string_lossy().replace('\\', "/"),
        output: volume.output_filename.clone(),
        status: "failed",
        built: false,
        resumed,
        entries: None,
        chapters: None,
        first_headword: None,
        last_headword: None,
        record_sha256: None,
        audit: "not-run",
        epubcheck: if options.epubcheck.is_some() {
            "not-run"
        } else {
            "skipped"
        },
        epubcheck_log: String::new(),
        error_code: None,
        error: None,
    };

    if !resumed {
        let build = match build_volume(volume, output_directory, options.build) {
            Ok(build) => build,
            Err(error) => {
                fail_build(&mut report, error);
                return report;
            }
        };
        apply_build(&mut report, &build);
    }

    let audit = match audit_volume(volume, output_directory) {
        Ok(audit) => audit,
        Err(error) => {
            fail_audit(&mut report, error);
            return report;
        }
    };
    apply_audit(&mut report, &audit);

    if let Some(configuration) = &options.epubcheck {
        match run_epubcheck(configuration, &output_path) {
            Ok(log) => {
                report.epubcheck = "passed";
                report.epubcheck_log = log;
            }
            Err((code, reason, log)) => {
                report.epubcheck = "failed";
                report.epubcheck_log = log;
                report.error_code = Some(code);
                report.error = Some(reason);
                return report;
            }
        }
    }

    report.status = "passed";
    report
}

fn apply_build(report: &mut CorpusBookReport, build: &BuildReport) {
    report.built = true;
    report.entries = Some(build.entries);
    report.chapters = Some(build.chapters);
    report.first_headword = Some(build.first_headword.clone());
    report.last_headword = Some(build.last_headword.clone());
    report.record_sha256 = Some(build.digest.sha256.clone());
}

fn apply_audit(report: &mut CorpusBookReport, audit: &AuditReport) {
    report.audit = "passed";
    report.entries = Some(audit.epub_summary.entries);
    report.first_headword = Some(audit.epub_summary.first_headword.clone());
    report.last_headword = Some(audit.epub_summary.last_headword.clone());
    report.record_sha256 = Some(audit.epub_summary.record_sha256.clone());
}

fn fail_build(report: &mut CorpusBookReport, error: EpubError) {
    report.error_code = Some(error.code());
    report.error = Some(error.to_string());
}

fn fail_audit(report: &mut CorpusBookReport, error: AuditError) {
    report.audit = "failed";
    report.error_code = Some(error.code());
    report.error = Some(error.to_string());
}

fn run_epubcheck(
    options: &EpubCheckOptions,
    output: &Path,
) -> Result<String, (&'static str, String, String)> {
    let completed = Command::new(&options.java)
        .arg("-jar")
        .arg(&options.jar)
        .arg(output)
        .arg("--failonwarnings")
        .arg("--quiet")
        .output()
        .map_err(|error| {
            (
                "KDEP-E012",
                format!("could not start EPUBCheck: {error}"),
                String::new(),
            )
        })?;
    let mut log = String::from_utf8_lossy(&completed.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&completed.stderr));
    if completed.status.success() {
        Ok(log)
    } else {
        Err((
            "KDEP-E013",
            format!(
                "EPUBCheck rejected '{}' with exit code {}",
                output
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| output.display().to_string()),
                completed
                    .status
                    .code()
                    .map_or_else(|| "terminated".to_owned(), |code| code.to_string())
            ),
            log,
        ))
    }
}

fn write_report(path: &Path, report: &CorpusReport) -> Result<(), BatchError> {
    let mut output = AtomicWriteFile::options().open(path)?;
    serde_json::to_writer_pretty(&mut output, report)?;
    output.write_all(b"\n")?;
    output.commit()?;
    Ok(())
}

const fn dictionary_order(dictionary: &str) -> u8 {
    match dictionary.as_bytes() {
        b"krdict" => 1,
        b"stdict" => 2,
        b"opendict" => 3,
        _ => 4,
    }
}
