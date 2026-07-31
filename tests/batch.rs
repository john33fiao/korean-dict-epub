use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::batch::{BatchError, BatchOptions, EpubCheckOptions, run_batch};
use korean_dict_epub::catalog::{Dictionary, Volume};
use korean_dict_epub::epub::BuildOptions;
use serde_json::Value;

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempFixture {
    root: PathBuf,
}

impl TempFixture {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "korean-dict-epub-batch-{}-{sequence}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("stale fixture should be removable");
        }
        fs::create_dir_all(&root).expect("fixture root should be created");
        Self { root }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn volumes(root: &Path, malformed: Option<usize>) -> Vec<Volume> {
    let source = root.join("source");
    fs::create_dir_all(&source).expect("source fixture directory should be created");
    (1..=3)
        .map(|number| {
            let path = source.join(format!("{number:03}.xml"));
            let xml = if malformed == Some(number) {
                "<LexicalResource><LexicalEntry></LexicalResource>".to_owned()
            } else {
                format!(
                    "<LexicalResource><Lexicon><LexicalEntry id=\"{number}\">\
                     <Lemma><feat att=\"writtenForm\" val=\"표제어 {number}\"/></Lemma>\
                     </LexicalEntry></Lexicon></LexicalResource>"
                )
            };
            fs::write(&path, xml).expect("source fixture should be written");
            Volume {
                dictionary: Dictionary::Krdict,
                number,
                total: 3,
                relative_source: PathBuf::from(format!("krdict/{number:03}.xml")),
                source: path,
                output_filename: format!("01-한국어기초사전-{number:03}-of-003.epub"),
            }
        })
        .collect()
}

fn options() -> BatchOptions {
    BatchOptions {
        jobs: 2,
        overwrite: false,
        resume: false,
        keep_going: false,
        build: BuildOptions {
            entries_per_chapter: 300,
            chapter_bytes: 1_048_576,
            overwrite: false,
        },
        epubcheck: None,
    }
}

#[test]
fn builds_audits_resumes_and_overwrites_a_small_corpus() {
    let fixture = TempFixture::new();
    let corpus = volumes(&fixture.root, None);
    let output = fixture.root.join("output");

    let first =
        run_batch(corpus.clone(), &output, options()).expect("small corpus should complete");

    assert_eq!(first.status, "partial");
    assert_eq!(first.expected_volumes, 3);
    assert_eq!(first.processed_volumes, 3);
    assert_eq!(first.passed_volumes, 3);
    assert_eq!(first.failed_volumes, 0);
    assert_eq!(first.total_entries, 3);
    assert!(first.books.iter().all(|book| book.built && !book.resumed));
    assert!(first.books.iter().all(|book| book.audit == "passed"));
    assert!(first.books.iter().all(|book| book.epubcheck == "skipped"));
    let report_path = output.join("corpus-report.json");
    let json: Value =
        serde_json::from_slice(&fs::read(&report_path).expect("corpus report should be written"))
            .expect("corpus report should be valid JSON");
    assert_eq!(json["schema"], "kdep-corpus-report-v1");
    assert_eq!(json["status"], "partial");

    let existing = run_batch(corpus.clone(), &output, options())
        .expect_err("existing outputs should be refused by default");
    assert!(matches!(existing, BatchError::ExistingOutputs(_)));

    let resumed = run_batch(
        corpus.clone(),
        &output,
        BatchOptions {
            resume: true,
            ..options()
        },
    )
    .expect("resume should audit existing outputs");
    assert!(resumed.books.iter().all(|book| book.resumed && !book.built));

    let overwritten = run_batch(
        corpus,
        &output,
        BatchOptions {
            overwrite: true,
            build: BuildOptions {
                overwrite: true,
                ..options().build
            },
            ..options()
        },
    )
    .expect("overwrite should rebuild every output");
    assert!(
        overwritten
            .books
            .iter()
            .all(|book| book.built && !book.resumed)
    );
}

#[test]
fn stop_and_keep_going_policies_are_reported_separately() {
    let stopped_fixture = TempFixture::new();
    let stopped_corpus = volumes(&stopped_fixture.root, Some(2));
    let stopped_output = stopped_fixture.root.join("output");

    let stopped = run_batch(
        stopped_corpus,
        &stopped_output,
        BatchOptions {
            jobs: 1,
            ..options()
        },
    )
    .expect_err("malformed second volume should fail the batch");
    assert!(matches!(stopped, BatchError::VolumeFailures { .. }));
    let stopped_report: Value = serde_json::from_slice(
        &fs::read(stopped_output.join("corpus-report.json")).expect("stopped report should exist"),
    )
    .expect("stopped report should be JSON");
    assert_eq!(stopped_report["processed_volumes"], 2);
    assert_eq!(stopped_report["failed_volumes"], 1);
    assert_eq!(stopped_report["unprocessed_volumes"], 1);
    assert!(
        !stopped_output
            .join("01-한국어기초사전-003-of-003.epub")
            .exists()
    );

    let continued_fixture = TempFixture::new();
    let continued_corpus = volumes(&continued_fixture.root, Some(2));
    let continued_output = continued_fixture.root.join("output");
    let continued = run_batch(
        continued_corpus,
        &continued_output,
        BatchOptions {
            jobs: 1,
            keep_going: true,
            ..options()
        },
    )
    .expect_err("failure should remain visible with keep-going");
    assert!(matches!(continued, BatchError::VolumeFailures { .. }));
    let continued_report: Value = serde_json::from_slice(
        &fs::read(continued_output.join("corpus-report.json"))
            .expect("continued report should exist"),
    )
    .expect("continued report should be JSON");
    assert_eq!(continued_report["processed_volumes"], 3);
    assert_eq!(continued_report["failed_volumes"], 1);
    assert_eq!(continued_report["unprocessed_volumes"], 0);
    assert!(
        continued_output
            .join("01-한국어기초사전-003-of-003.epub")
            .is_file()
    );
}

#[test]
fn invalid_epubcheck_jar_fails_before_creating_outputs() {
    let fixture = TempFixture::new();
    let corpus = volumes(&fixture.root, None);
    let output = fixture.root.join("output");
    let missing = fixture.root.join("missing-epubcheck.jar");

    let error = run_batch(
        corpus,
        &output,
        BatchOptions {
            epubcheck: Some(EpubCheckOptions {
                java: PathBuf::from("java"),
                jar: missing,
            }),
            ..options()
        },
    )
    .expect_err("missing EPUBCheck jar should fail setup");

    assert!(matches!(error, BatchError::InvalidEpubCheckJar(_)));
    assert!(!output.exists());
}
