use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempFixture {
    root: PathBuf,
}

impl TempFixture {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "korean-dict-epub-cli-{}-{sequence}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("stale fixture should be removable");
        }
        fs::create_dir_all(&root).expect("fixture root should be created");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn source_with_all_dictionaries(&self) -> PathBuf {
        let source = self.root.join("source");
        for dictionary in ["krdict", "stdict", "opendict"] {
            fs::create_dir_all(source.join(dictionary))
                .expect("dictionary fixture directory should be created");
        }
        source
    }

    fn tracked_krdict_source(&self, xml: &str) -> PathBuf {
        let source = self.root.join("tracked-source");
        fs::create_dir_all(source.join("krdict"))
            .expect("tracked source directory should be created");
        fs::write(source.join("krdict").join("001.xml"), xml)
            .expect("tracked XML should be written");
        fs::write(
            source.join("krdict").join("999.xml"),
            "<LexicalResource><LexicalEntry/></LexicalResource>",
        )
        .expect("untracked XML should be written");

        let init = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&source)
            .status()
            .expect("Git should initialize the fixture");
        assert!(init.success());
        let add = Command::new("git")
            .arg("-C")
            .arg(&source)
            .arg("add")
            .arg("--")
            .arg("krdict/001.xml")
            .status()
            .expect("Git should track the fixture");
        assert!(add.success());
        source
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_korean-dict-epub"))
        .args(arguments)
        .output()
        .expect("CLI should start")
}

#[test]
fn help_exits_successfully() {
    let output = run(&["--help"]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("preflight"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("inspect"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("build"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("audit"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("batch"));
}

#[test]
fn empty_arguments_show_help_without_panicking() {
    let output = run(&[]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));
}

#[test]
fn invalid_dictionary_is_a_usage_error() {
    let output = run(&["preflight", "--dictionary", "unknown"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value"));
}

#[test]
fn missing_source_has_structured_runtime_error() {
    let fixture = TempFixture::new();
    let missing = fixture.path().join("missing");
    let output_path = fixture.path().join("output");
    let output = run(&[
        "preflight",
        "--source",
        &missing.to_string_lossy(),
        "--output",
        &output_path.to_string_lossy(),
    ]);

    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("error[KDEP-E001]"));
}

#[test]
fn fixture_preflight_reports_policy_without_writing_output() {
    let fixture = TempFixture::new();
    let source = fixture.source_with_all_dictionaries();
    let output_path = fixture.path().join("output");
    let output = run(&[
        "preflight",
        "--source",
        &source.to_string_lossy(),
        "--output",
        &output_path.to_string_lossy(),
        "--dictionary",
        "all",
        "--jobs",
        "2",
    ]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status=ready"));
    assert!(stdout.contains("dictionary=all"));
    assert!(stdout.contains("jobs=2"));
    assert!(stdout.contains("overwrite=false"));
    assert!(stdout.contains("keep_going=false"));
    assert!(!output_path.exists());
}

#[test]
fn inspect_reads_one_tracked_volume_without_writing_files() {
    let fixture = TempFixture::new();
    let source = fixture.tracked_krdict_source(
        "<LexicalResource><Lexicon><LexicalEntry id=\"one\"><future/></LexicalEntry>\
         </Lexicon></LexicalResource>",
    );
    let output = run(&[
        "inspect",
        "--source",
        &source.to_string_lossy(),
        "--dictionary",
        "krdict",
        "--volume",
        "1",
    ]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status=inspected"));
    assert!(stdout.contains("dictionary=krdict"));
    assert!(stdout.contains("volumes=1"));
    assert!(stdout.contains("source=krdict\\001.xml") || stdout.contains("source=krdict/001.xml"));
    assert!(stdout.contains("entries=1"));
    assert!(stdout.contains("record_schema=kdep-source-record-v1"));
    assert!(stdout.contains("record_sha256="));
    assert!(!fixture.path().join("outputs").exists());
}

#[test]
fn inspect_excludes_untracked_xml_from_volume_numbers() {
    let fixture = TempFixture::new();
    let source = fixture.tracked_krdict_source(
        "<LexicalResource><Lexicon><LexicalEntry/></Lexicon></LexicalResource>",
    );
    let output = run(&[
        "inspect",
        "--source",
        &source.to_string_lossy(),
        "--dictionary",
        "krdict",
        "--volume",
        "2",
    ]);

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error[KDEP-E006]"));
    assert!(stderr.contains("available range is 1..=1"));
}

#[test]
fn inspect_reports_malformed_xml_with_structured_error() {
    let fixture = TempFixture::new();
    let source = fixture.tracked_krdict_source("<LexicalResource><LexicalEntry></LexicalResource>");
    let output = run(&[
        "inspect",
        "--source",
        &source.to_string_lossy(),
        "--dictionary",
        "krdict",
    ]);

    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("error[KDEP-E007]"));
}

#[test]
fn build_writes_one_epub_and_requires_explicit_overwrite() {
    let fixture = TempFixture::new();
    let source = fixture.tracked_krdict_source(
        "<LexicalResource><Lexicon><LexicalEntry><Lemma>\
         <feat att=\"writtenForm\" val=\"가상어\"/></Lemma></LexicalEntry>\
         </Lexicon></LexicalResource>",
    );
    let output_path = fixture.path().join("epubs");
    let source_argument = source.to_string_lossy().into_owned();
    let output_argument = output_path.to_string_lossy().into_owned();
    let arguments = [
        "build",
        "--source",
        source_argument.as_str(),
        "--output",
        output_argument.as_str(),
        "--dictionary",
        "krdict",
    ];

    let built = run(&arguments);

    assert!(built.status.success());
    assert!(String::from_utf8_lossy(&built.stdout).contains("status=built"));
    let epub = output_path.join("01-한국어기초사전-001-of-001.epub");
    assert!(epub.is_file());

    let refused = run(&arguments);
    assert_eq!(refused.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("error[KDEP-E008]"));

    let mut overwrite_arguments = arguments.to_vec();
    overwrite_arguments.push("--overwrite");
    let overwritten = run(&overwrite_arguments);
    assert!(overwritten.status.success());

    let audit_arguments = [
        "audit",
        "--source",
        source_argument.as_str(),
        "--output",
        output_argument.as_str(),
        "--dictionary",
        "krdict",
    ];
    let audited = run(&audit_arguments);
    assert!(audited.status.success());
    assert!(String::from_utf8_lossy(&audited.stdout).contains("status=passed"));
    assert!(
        output_path
            .join("01-한국어기초사전-001-of-001.epub.audit.json")
            .is_file()
    );

    fs::write(
        source.join("krdict").join("001.xml"),
        "<LexicalResource><Lexicon><LexicalEntry><Lemma>\
         <feat att=\"writtenForm\" val=\"변경어\"/></Lemma></LexicalEntry>\
         </Lexicon></LexicalResource>",
    )
    .expect("tracked source fixture should be mutable");
    let mismatched = run(&audit_arguments);
    assert_eq!(mismatched.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&mismatched.stderr).contains("error[KDEP-E011]"));
}

#[test]
fn batch_writes_partial_report_and_requires_resume_for_existing_outputs() {
    let fixture = TempFixture::new();
    let source = fixture.tracked_krdict_source(
        "<LexicalResource><Lexicon><LexicalEntry><Lemma>\
         <feat att=\"writtenForm\" val=\"배치어\"/></Lemma></LexicalEntry>\
         </Lexicon></LexicalResource>",
    );
    let output_path = fixture.path().join("batch-output");
    let source_argument = source.to_string_lossy().into_owned();
    let output_argument = output_path.to_string_lossy().into_owned();
    let arguments = [
        "batch",
        "--source",
        source_argument.as_str(),
        "--output",
        output_argument.as_str(),
        "--dictionary",
        "krdict",
    ];

    let first = run(&arguments);
    assert!(first.status.success());
    assert!(String::from_utf8_lossy(&first.stdout).contains("status=partial"));
    assert!(output_path.join("corpus-report.json").is_file());

    let refused = run(&arguments);
    assert_eq!(refused.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("error[KDEP-E012]"));

    let mut resume_arguments = arguments.to_vec();
    resume_arguments.push("--resume");
    let resumed = run(&resume_arguments);
    assert!(resumed.status.success());
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("status=partial"));
}
