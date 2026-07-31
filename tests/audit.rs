use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::audit::{AuditError, audit_volume};
use korean_dict_epub::catalog::{Dictionary, Volume};
use korean_dict_epub::epub::{BuildOptions, build_volume};
use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
type ChapterMutation = (&'static str, fn(&str) -> String);

struct TempFixture {
    root: PathBuf,
}

impl TempFixture {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "korean-dict-epub-audit-{}-{sequence}",
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

fn source_fixture(path: &Path) {
    let mut xml = Vec::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <LexicalResource><Lexicon><meta>헤더 값</meta>\
         <LexicalEntry id=\"one\"><Lemma>\
         <feat att=\"writtenForm\" val=\"첫째\"/></Lemma>\
         <future:opaque xmlns:future=\"urn:kdep:fixture\" zeta=\"첫째\" alpha=\"둘째\">\
         미지 값</future:opaque></LexicalEntry>\
         <LexicalEntry id=\"two\"><Lemma>\
         <feat att=\"writtenForm\" val=\"둘째\"/></Lemma>\
         <unknown value=\"앞"
            .as_bytes(),
    );
    xml.push(0x08);
    xml.extend_from_slice("뒤\"/></LexicalEntry></Lexicon></LexicalResource>".as_bytes());
    fs::write(path, xml).expect("source fixture should be written");
}

fn fixture_volume(source: PathBuf) -> Volume {
    Volume {
        dictionary: Dictionary::Krdict,
        number: 1,
        total: 1,
        relative_source: PathBuf::from("krdict/001.xml"),
        source,
        output_filename: "01-한국어기초사전-001-of-001.epub".to_owned(),
    }
}

fn build_fixture() -> (TempFixture, Volume, PathBuf) {
    let fixture = TempFixture::new();
    let source = fixture.root.join("source.xml");
    source_fixture(&source);
    let output = fixture.root.join("output");
    let volume = fixture_volume(source);
    build_volume(
        &volume,
        &output,
        BuildOptions {
            entries_per_chapter: 300,
            chapter_bytes: 1_048_576,
            overwrite: false,
        },
    )
    .expect("fixture EPUB should build");
    (fixture, volume, output)
}

#[test]
fn independently_audits_source_epub_metadata_and_deterministic_report() {
    let (_fixture, volume, output) = build_fixture();

    let report = audit_volume(&volume, &output).expect("independent audit should pass");

    assert_eq!(report.status, "passed");
    assert_eq!(
        report.source_summary.record_sha256,
        report.epub_summary.record_sha256
    );
    assert_eq!(report.source_summary.entries, 2);
    assert_eq!(report.epub_summary.entries, 2);
    assert_eq!(report.epub_summary.first_headword, "첫째");
    assert_eq!(report.epub_summary.last_headword, "둘째");
    assert!(report.checks.iter().all(|check| check.passed));
    let report_path = output.join("01-한국어기초사전-001-of-001.epub.audit.json");
    let first = fs::read(&report_path).expect("JSON report should exist");
    let json: Value = serde_json::from_slice(&first).expect("report should be valid JSON");
    assert_eq!(json["schema"], "kdep-audit-report-v1");
    assert_eq!(json["status"], "passed");

    audit_volume(&volume, &output).expect("repeated audit should pass");
    assert_eq!(
        fs::read(report_path).expect("repeated report should exist"),
        first,
        "audit report should be deterministic"
    );
}

#[test]
fn detects_field_omission_entry_order_attribute_and_control_mutations() {
    let mutations: [ChapterMutation; 5] = [
        ("field-omission", omit_first_record),
        ("entry-order", swap_entries),
        ("attribute-value", change_attribute_value),
        ("attribute-order", swap_attributes),
        ("control-codepoint", change_control_codepoint),
    ];

    for (name, mutation) in mutations {
        let (_fixture, volume, output) = build_fixture();
        let epub = output.join(&volume.output_filename);
        mutate_first_chapter(&epub, mutation);

        let error = audit_volume(&volume, &output)
            .expect_err("intentional XHTML mutation must fail independent audit");
        assert!(
            matches!(error, AuditError::Mismatch { .. }),
            "{name} should produce a content mismatch, got {error}"
        );
        let report_path = output.join(format!("{}.audit.json", volume.output_filename));
        let json: Value = serde_json::from_slice(
            &fs::read(report_path).expect("failure report should be written"),
        )
        .expect("failure report should be valid JSON");
        assert_eq!(json["status"], "failed", "{name}");
        assert!(
            json["checks"]
                .as_array()
                .expect("checks should be an array")
                .iter()
                .any(|check| check["name"] == "record_sha256" && check["passed"] == false),
            "{name} should fail the record digest check"
        );
    }
}

fn mutate_first_chapter(epub: &Path, mutation: fn(&str) -> String) {
    let input = fs::File::open(epub).expect("EPUB should open");
    let mut archive = ZipArchive::new(input).expect("EPUB should be a ZIP");
    let temporary = epub.with_extension("mutated.epub");
    let output = fs::File::create(&temporary).expect("mutated EPUB should be created");
    let mut writer = ZipWriter::new(output);
    let mut changed = false;

    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("ZIP member should open");
        let name = member.name().to_owned();
        let compression = member.compression();
        let mut bytes = Vec::new();
        member
            .read_to_end(&mut bytes)
            .expect("ZIP member should be readable");
        if !changed && name.contains("/chapter-") {
            let original = String::from_utf8(bytes).expect("chapter should be UTF-8");
            let mutated = mutation(&original);
            assert_ne!(mutated, original, "mutation must alter its target");
            bytes = mutated.into_bytes();
            changed = true;
        }
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(compression),
            )
            .expect("mutated member should start");
        writer
            .write_all(&bytes)
            .expect("mutated member should be written");
    }
    writer.finish().expect("mutated ZIP should finish");
    assert!(changed, "a chapter should have been mutated");
    fs::remove_file(epub).expect("original fixture EPUB should be removable");
    fs::rename(temporary, epub).expect("mutated EPUB should replace fixture output");
}

fn omit_first_record(chapter: &str) -> String {
    let start = chapter
        .find("<div class=\"xml-record")
        .expect("record start should exist");
    let relative_end = chapter[start..]
        .find("</div>\n")
        .expect("record end should exist");
    let end = start + relative_end + "</div>\n".len();
    format!("{}{}", &chapter[..start], &chapter[end..])
}

fn swap_entries(chapter: &str) -> String {
    let first_start = chapter
        .find("<article class=\"entry\"")
        .expect("first entry should exist");
    let first_end = first_start
        + chapter[first_start..]
            .find("</article>\n")
            .expect("first entry should end")
        + "</article>\n".len();
    let second_start = first_end
        + chapter[first_end..]
            .find("<article class=\"entry\"")
            .expect("second entry should exist");
    let second_end = second_start
        + chapter[second_start..]
            .find("</article>\n")
            .expect("second entry should end")
        + "</article>\n".len();
    format!(
        "{}{}{}{}{}",
        &chapter[..first_start],
        &chapter[second_start..second_end],
        &chapter[first_end..second_start],
        &chapter[first_start..first_end],
        &chapter[second_end..]
    )
}

fn change_attribute_value(chapter: &str) -> String {
    chapter.replacen(
        "<span class=\"xml-attribute-value\">첫째</span>",
        "<span class=\"xml-attribute-value\">손상</span>",
        1,
    )
}

fn swap_attributes(chapter: &str) -> String {
    let zeta = attribute_markup(chapter, "zeta");
    let alpha = attribute_markup(chapter, "alpha");
    assert!(
        zeta.0 < alpha.0,
        "fixture attribute order should be zeta, alpha"
    );
    format!(
        "{}{}{}{}{}",
        &chapter[..zeta.0],
        &chapter[alpha.0..alpha.1],
        &chapter[zeta.1..alpha.0],
        &chapter[zeta.0..zeta.1],
        &chapter[alpha.1..]
    )
}

fn attribute_markup(chapter: &str, name: &str) -> (usize, usize) {
    let needle =
        format!("<span class=\"xml-attribute\"><code class=\"xml-attribute-name\">{name}</code>");
    let start = chapter.find(&needle).expect("named attribute should exist");
    let end = start
        + chapter[start..]
            .find("</span></span>")
            .expect("attribute markup should end")
        + "</span></span>".len();
    (start, end)
}

fn change_control_codepoint(chapter: &str) -> String {
    chapter.replacen("data-codepoint=\"U+0008\"", "data-codepoint=\"U+0007\"", 1)
}
