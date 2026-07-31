use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::catalog::{Dictionary, Volume};
use korean_dict_epub::epub::{BuildOptions, EpubError, build_volume};
use quick_xml::events::Event;
use quick_xml::reader::Reader as XmlReader;
use zip::{CompressionMethod, ZipArchive};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempFixture {
    root: PathBuf,
}

impl TempFixture {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "korean-dict-epub-package-{}-{sequence}",
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
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn source_fixture(path: &Path) {
    let mut xml = Vec::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <LexicalResource><Lexicon><meta>합성 헤더</meta>\
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

fn volume(source: PathBuf) -> Volume {
    Volume {
        dictionary: Dictionary::Krdict,
        number: 1,
        total: 1,
        relative_source: PathBuf::from("krdict/001.xml"),
        source,
        output_filename: "01-한국어기초사전-001-of-001.epub".to_owned(),
    }
}

fn assert_well_formed_xml(bytes: &[u8]) {
    let mut reader = XmlReader::from_reader(Cursor::new(bytes));
    reader.config_mut().enable_all_checks(true);
    let mut buffer = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .expect("generated XML should parse")
        {
            Event::Eof => break,
            _ => buffer.clear(),
        }
    }
}

fn archive_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).expect("EPUB should be readable")
}

#[test]
fn builds_deterministic_epub_with_complete_record_markup() {
    let fixture = TempFixture::new();
    let source = fixture.path().join("source.xml");
    source_fixture(&source);
    let output = fixture.path().join("output");
    let volume = volume(source);
    let options = BuildOptions {
        entries_per_chapter: 1,
        chapter_bytes: 1_048_576,
        overwrite: false,
    };

    let report = build_volume(&volume, &output, options).expect("EPUB should build");

    assert_eq!(report.entries, 2);
    assert_eq!(report.chapters, 2);
    assert_eq!(report.first_headword, "첫째");
    assert_eq!(report.last_headword, "둘째");
    let first_bytes = archive_bytes(&report.output);

    let file = fs::File::open(&report.output).expect("EPUB should open");
    let mut archive = ZipArchive::new(file).expect("EPUB should be a ZIP archive");
    assert_eq!(
        archive.by_index(0).expect("mimetype should exist").name(),
        "mimetype"
    );
    assert_eq!(
        archive
            .by_name("mimetype")
            .expect("mimetype should exist")
            .compression(),
        CompressionMethod::Stored
    );

    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    assert_eq!(
        names,
        [
            "mimetype",
            "META-INF/container.xml",
            "EPUB/package.opf",
            "EPUB/nav.xhtml",
            "EPUB/styles/book.css",
            "EPUB/text/title.xhtml",
            "EPUB/text/chapter-0001.xhtml",
            "EPUB/text/chapter-0002.xhtml",
        ]
    );

    let mut record_count = 0_usize;
    let mut chapter_documents = Vec::new();
    for name in names.iter().filter(|name| name.ends_with(".xhtml")) {
        let mut content = Vec::new();
        archive
            .by_name(name)
            .expect("XHTML member should exist")
            .read_to_end(&mut content)
            .expect("XHTML should be readable");
        assert_well_formed_xml(&content);
        let text = String::from_utf8(content).expect("XHTML should be UTF-8");
        if name.contains("/chapter-") {
            record_count += text.matches("data-kdep-record=\"true\"").count();
            chapter_documents.push(text);
        }
    }
    assert_eq!(
        u64::try_from(record_count).expect("record count should fit"),
        report.record_count
    );
    assert!(
        chapter_documents
            .iter()
            .any(|chapter| chapter.contains("data-codepoint=\"U+0008\""))
    );
    assert!(
        chapter_documents
            .iter()
            .any(|chapter| chapter.contains("future:opaque"))
    );

    let mut opf = String::new();
    archive
        .by_name("EPUB/package.opf")
        .expect("package should exist")
        .read_to_string(&mut opf)
        .expect("package should be UTF-8");
    assert_well_formed_xml(opf.as_bytes());
    let mut nav = String::new();
    archive
        .by_name("EPUB/nav.xhtml")
        .expect("nav should exist")
        .read_to_string(&mut nav)
        .expect("nav should be UTF-8");
    for chapter in ["chapter-0001.xhtml", "chapter-0002.xhtml"] {
        assert!(opf.contains(chapter));
        assert!(nav.contains(chapter));
    }

    let error = build_volume(&volume, &output, options)
        .expect_err("existing output should be refused by default");
    assert!(matches!(error, EpubError::OutputExists(_)));
    assert_eq!(archive_bytes(&report.output), first_bytes);

    let overwritten = build_volume(
        &volume,
        &output,
        BuildOptions {
            overwrite: true,
            ..options
        },
    )
    .expect("explicit overwrite should succeed");
    assert_eq!(archive_bytes(&overwritten.output), first_bytes);

    let output_files: Vec<_> = fs::read_dir(&output)
        .expect("output directory should exist")
        .map(|entry| entry.expect("output entry should be readable").file_name())
        .collect();
    assert_eq!(output_files.len(), 1);
}

#[test]
#[ignore = "requires Pandoc on PATH"]
fn pandoc_reopens_small_epub_fixture() {
    let fixture = TempFixture::new();
    let source = fixture.path().join("source.xml");
    source_fixture(&source);
    let output = fixture.path().join("output");
    let report = build_volume(
        &volume(source),
        &output,
        BuildOptions {
            entries_per_chapter: 1,
            chapter_bytes: 1_048_576,
            overwrite: false,
        },
    )
    .expect("EPUB should build");
    let reopened = fixture.path().join("pandoc.txt");

    let status = Command::new("pandoc")
        .arg(&report.output)
        .arg("--to=plain")
        .arg("--output")
        .arg(&reopened)
        .status()
        .expect("Pandoc should start");

    assert!(status.success(), "Pandoc should reopen the EPUB fixture");
    assert!(
        fs::metadata(reopened)
            .expect("Pandoc output should exist")
            .len()
            > 0
    );
}
