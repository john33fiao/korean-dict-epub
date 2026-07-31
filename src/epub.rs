use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use atomic_write_file::AtomicWriteFile;
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::catalog::{Dictionary, Volume};
use crate::record::{CanonicalDigest, DigestSummary, SourceAttribute, SourceRecord};
use crate::render::{
    BOOK_CSS, escape_xml, local_name, render_record, render_value, xhtml_document,
};
use crate::source::{SourceError, SourceRecordReader};

pub const DEFAULT_ENTRIES_PER_CHAPTER: usize = 300;
pub const DEFAULT_CHAPTER_BYTES: usize = 1_048_576;
const MODIFIED_TIMESTAMP: &str = "1980-01-01T00:00:00Z";
const BOOK_NAMESPACE: Uuid = Uuid::from_u128(0x34a87ea1f0e44f4d88941de178aa7a3e);
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const EPUB_NS: &str = "http://www.idpf.org/2007/ops";
const OPF_NS: &str = "http://www.idpf.org/2007/opf";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const CONTAINER_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:container";

static STAGE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildOptions {
    pub entries_per_chapter: usize,
    pub chapter_bytes: usize,
    pub overwrite: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            entries_per_chapter: DEFAULT_ENTRIES_PER_CHAPTER,
            chapter_bytes: DEFAULT_CHAPTER_BYTES,
            overwrite: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    pub dictionary: Dictionary,
    pub volume: usize,
    pub volumes: usize,
    pub source: PathBuf,
    pub output: PathBuf,
    pub entries: u64,
    pub chapters: usize,
    pub first_headword: String,
    pub last_headword: String,
    pub record_count: u64,
    pub digest: DigestSummary,
}

impl fmt::Display for BuildReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "status=built")?;
        writeln!(formatter, "dictionary={}", self.dictionary.key())?;
        writeln!(formatter, "volume={}", self.volume)?;
        writeln!(formatter, "volumes={}", self.volumes)?;
        writeln!(formatter, "source={}", self.source.display())?;
        writeln!(formatter, "output={}", self.output.display())?;
        writeln!(formatter, "entries={}", self.entries)?;
        writeln!(formatter, "chapters={}", self.chapters)?;
        writeln!(formatter, "first_headword={}", self.first_headword)?;
        writeln!(formatter, "last_headword={}", self.last_headword)?;
        writeln!(formatter, "record_count={}", self.record_count)?;
        writeln!(formatter, "record_schema={}", self.digest.schema)?;
        write!(formatter, "record_sha256={}", self.digest.sha256)
    }
}

#[derive(Debug)]
pub enum EpubError {
    OutputExists(PathBuf),
    InvalidOptions(&'static str),
    NoEntries(PathBuf),
    UnclosedEntry,
    Io(io::Error),
    Source(SourceError),
    Zip(zip::result::ZipError),
    PackageMismatch(String),
}

impl EpubError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::OutputExists(_) => "KDEP-E008",
            Self::InvalidOptions(_)
            | Self::NoEntries(_)
            | Self::UnclosedEntry
            | Self::Io(_)
            | Self::Source(_)
            | Self::Zip(_)
            | Self::PackageMismatch(_) => "KDEP-E009",
        }
    }
}

impl fmt::Display for EpubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputExists(path) => {
                write!(
                    formatter,
                    "output '{}' already exists; pass --overwrite to replace it",
                    path.display()
                )
            }
            Self::InvalidOptions(reason) => write!(formatter, "invalid build options: {reason}"),
            Self::NoEntries(path) => {
                write!(
                    formatter,
                    "no dictionary entries were found in '{}'",
                    path.display()
                )
            }
            Self::UnclosedEntry => formatter.write_str("XML ended inside a dictionary entry"),
            Self::Io(error) => write!(formatter, "EPUB I/O error: {error}"),
            Self::Source(error) => write!(formatter, "source XML error: {error}"),
            Self::Zip(error) => write!(formatter, "EPUB ZIP error: {error}"),
            Self::PackageMismatch(reason) => {
                write!(
                    formatter,
                    "generated EPUB package is inconsistent: {reason}"
                )
            }
        }
    }
}

impl Error for EpubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::Zip(error) => Some(error),
            Self::OutputExists(_)
            | Self::InvalidOptions(_)
            | Self::NoEntries(_)
            | Self::UnclosedEntry
            | Self::PackageMismatch(_) => None,
        }
    }
}

impl From<io::Error> for EpubError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SourceError> for EpubError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

impl From<zip::result::ZipError> for EpubError {
    fn from(error: zip::result::ZipError) -> Self {
        Self::Zip(error)
    }
}

#[derive(Debug, Clone)]
struct ChapterInfo {
    filename: String,
    records: u64,
    entries: u64,
    first_entry: Option<u64>,
    last_entry: Option<u64>,
}

struct ChapterWriter {
    filename: String,
    writer: BufWriter<File>,
    content_bytes: usize,
    records: u64,
    entries: u64,
    first_entry: Option<u64>,
    last_entry: Option<u64>,
}

impl ChapterWriter {
    fn create(text_directory: &Path, number: usize, title: &str) -> Result<Self, EpubError> {
        let filename = format!("chapter-{number:04}.xhtml");
        let path = text_directory.join(&filename);
        let mut writer = BufWriter::new(File::create(path)?);
        let chapter_title = format!("{title} · {number}장");
        write!(
            writer,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"{XHTML_NS}\" xmlns:epub=\"{EPUB_NS}\" \
             lang=\"ko\" xml:lang=\"ko\">\n<head>\n<meta charset=\"UTF-8\" />\n\
             <title>{}</title>\n\
             <link rel=\"stylesheet\" type=\"text/css\" href=\"../styles/book.css\" />\n\
             </head>\n<body>\n<main epub:type=\"bodymatter\">\n\
             <h1 class=\"chapter-title\">{}</h1>\n",
            escape_xml(&chapter_title),
            escape_xml(&chapter_title)
        )?;
        Ok(Self {
            filename,
            writer,
            content_bytes: 0,
            records: 0,
            entries: 0,
            first_entry: None,
            last_entry: None,
        })
    }

    fn should_split(&self, fragment_bytes: usize, is_entry: bool, options: BuildOptions) -> bool {
        self.records > 0
            && ((is_entry && self.entries as usize >= options.entries_per_chapter)
                || self.content_bytes.saturating_add(fragment_bytes) > options.chapter_bytes)
    }

    fn write_fragment(
        &mut self,
        fragment: &str,
        records: usize,
        entry_number: Option<u64>,
    ) -> Result<(), EpubError> {
        self.writer.write_all(fragment.as_bytes())?;
        self.content_bytes = self.content_bytes.saturating_add(fragment.len());
        self.records += u64::try_from(records).expect("record count should fit in u64");
        if let Some(entry_number) = entry_number {
            self.entries += 1;
            self.first_entry.get_or_insert(entry_number);
            self.last_entry = Some(entry_number);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ChapterInfo, EpubError> {
        self.writer.write_all(b"</main>\n</body>\n</html>\n")?;
        self.writer.flush()?;
        Ok(ChapterInfo {
            filename: self.filename,
            records: self.records,
            entries: self.entries,
            first_entry: self.first_entry,
            last_entry: self.last_entry,
        })
    }
}

struct StageDirectory {
    path: PathBuf,
    active: bool,
}

impl StageDirectory {
    fn create(output_directory: &Path) -> Result<Self, EpubError> {
        fs::create_dir_all(output_directory)?;
        for _ in 0..100 {
            let sequence = STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = output_directory.join(format!(
                ".korean-dict-epub-stage-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, active: true }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(EpubError::Io(error)),
            }
        }
        Err(EpubError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique EPUB staging directory",
        )))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(mut self) -> Result<(), EpubError> {
        fs::remove_dir_all(&self.path)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for StageDirectory {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn build_volume(
    volume: &Volume,
    output_directory: &Path,
    options: BuildOptions,
) -> Result<BuildReport, EpubError> {
    if options.entries_per_chapter == 0 {
        return Err(EpubError::InvalidOptions(
            "entries per chapter must be at least one",
        ));
    }
    if options.chapter_bytes == 0 {
        return Err(EpubError::InvalidOptions(
            "chapter byte limit must be at least one",
        ));
    }

    let output_path = output_directory.join(&volume.output_filename);
    if output_path.exists() && !options.overwrite {
        return Err(EpubError::OutputExists(output_path));
    }

    let stage = StageDirectory::create(output_directory)?;
    let epub_directory = stage.path().join("EPUB");
    let text_directory = epub_directory.join("text");
    fs::create_dir_all(&text_directory)?;
    fs::create_dir_all(epub_directory.join("styles"))?;
    fs::create_dir_all(stage.path().join("META-INF"))?;
    fs::write(epub_directory.join("styles").join("book.css"), BOOK_CSS)?;
    fs::write(
        stage.path().join("META-INF").join("container.xml"),
        container_xml(),
    )?;

    let title = format!(
        "{} {:03}/{:03}",
        volume.dictionary.series(),
        volume.number,
        volume.total
    );
    let mut chapter = ChapterWriter::create(&text_directory, 1, &title)?;
    let mut chapters = Vec::new();
    let mut digest = CanonicalDigest::new();
    let mut entry_buffer: Option<(usize, Vec<SourceRecord>)> = None;
    let mut entry_count = 0_u64;
    let mut first_headword = String::new();
    let mut last_headword = String::new();

    let source = File::open(&volume.source)?;
    for record in SourceRecordReader::new(source) {
        let record = record?;
        digest.update(&record);

        if let Some((entry_depth, records)) = entry_buffer.as_mut() {
            let is_end =
                matches!(&record, SourceRecord::EndElement { depth, .. } if depth == entry_depth);
            records.push(record);
            if is_end {
                let (_, records) = entry_buffer.take().expect("entry buffer exists");
                entry_count += 1;
                let headword = extract_headword(volume.dictionary, &records, entry_count);
                if first_headword.is_empty() {
                    first_headword = headword.clone();
                }
                last_headword = headword.clone();
                let fragment = render_entry(entry_count, &headword, &records);
                write_fragment_with_split(
                    &mut chapter,
                    &mut chapters,
                    &text_directory,
                    &title,
                    &fragment,
                    records.len(),
                    Some(entry_count),
                    options,
                )?;
            }
            continue;
        }

        if matches!(
            &record,
            SourceRecord::StartElement { name, .. }
                if volume.dictionary.is_entry_element(name)
        ) {
            entry_buffer = Some((record.depth(), vec![record]));
            continue;
        }
        if matches!(
            &record,
            SourceRecord::EmptyElement { name, .. }
                if volume.dictionary.is_entry_element(name)
        ) {
            entry_count += 1;
            let headword = extract_headword(
                volume.dictionary,
                std::slice::from_ref(&record),
                entry_count,
            );
            if first_headword.is_empty() {
                first_headword = headword.clone();
            }
            last_headword = headword.clone();
            let fragment = render_entry(entry_count, &headword, std::slice::from_ref(&record));
            write_fragment_with_split(
                &mut chapter,
                &mut chapters,
                &text_directory,
                &title,
                &fragment,
                1,
                Some(entry_count),
                options,
            )?;
            continue;
        }

        let fragment = render_record(&record);
        write_fragment_with_split(
            &mut chapter,
            &mut chapters,
            &text_directory,
            &title,
            &fragment,
            1,
            None,
            options,
        )?;
    }

    if entry_buffer.is_some() {
        return Err(EpubError::UnclosedEntry);
    }
    if entry_count == 0 {
        return Err(EpubError::NoEntries(volume.source.clone()));
    }
    chapters.push(chapter.finish()?);

    let digest = digest.finalize();
    let record_count = total_records(&digest);
    let source_name = volume.relative_source.to_string_lossy().replace('\\', "/");
    let identifier_key = format!("{}/{}", volume.dictionary.key(), source_name);
    let identifier = format!(
        "urn:uuid:{}",
        Uuid::new_v5(&BOOK_NAMESPACE, identifier_key.as_bytes())
    );

    fs::write(
        text_directory.join("title.xhtml"),
        title_xhtml(
            &title,
            volume,
            entry_count,
            &first_headword,
            &last_headword,
            &digest,
        ),
    )?;
    fs::write(
        epub_directory.join("nav.xhtml"),
        nav_xhtml(&title, &chapters),
    )?;
    fs::write(
        epub_directory.join("package.opf"),
        package_opf(&title, volume, &source_name, &identifier, &chapters),
    )?;

    let expected_members = package_members(&chapters);
    let atomic_output = package_stage(stage.path(), &output_path, &expected_members)?;
    stage.cleanup()?;
    if output_path.exists() && !options.overwrite {
        atomic_output.discard()?;
        return Err(EpubError::OutputExists(output_path));
    }
    atomic_output.commit()?;

    Ok(BuildReport {
        dictionary: volume.dictionary,
        volume: volume.number,
        volumes: volume.total,
        source: volume.relative_source.clone(),
        output: output_path,
        entries: entry_count,
        chapters: chapters.len(),
        first_headword,
        last_headword,
        record_count,
        digest,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_fragment_with_split(
    chapter: &mut ChapterWriter,
    chapters: &mut Vec<ChapterInfo>,
    text_directory: &Path,
    title: &str,
    fragment: &str,
    records: usize,
    entry_number: Option<u64>,
    options: BuildOptions,
) -> Result<(), EpubError> {
    if chapter.should_split(fragment.len(), entry_number.is_some(), options) {
        let next = ChapterWriter::create(text_directory, chapters.len() + 2, title)?;
        let previous = std::mem::replace(chapter, next);
        chapters.push(previous.finish()?);
    }
    chapter.write_fragment(fragment, records, entry_number)
}

fn render_entry(number: u64, headword: &str, records: &[SourceRecord]) -> String {
    let mut fragment = format!(
        "<article class=\"entry\" id=\"entry-{number:07}\">\n\
         <h2 class=\"entry-heading\">{}</h2>\n<div class=\"entry-records\">\n",
        render_value(headword)
    );
    for record in records {
        fragment.push_str(&render_record(record));
    }
    fragment.push_str("</div>\n</article>\n");
    fragment
}

fn extract_headword(dictionary: Dictionary, records: &[SourceRecord], index: u64) -> String {
    if dictionary == Dictionary::Krdict {
        for record in records {
            let attributes = match record {
                SourceRecord::StartElement {
                    name, attributes, ..
                }
                | SourceRecord::EmptyElement {
                    name, attributes, ..
                } if local_name(name) == "feat" => attributes,
                _ => continue,
            };
            if attribute_value(attributes, "att") == Some("writtenForm")
                && let Some(value) = attribute_value(attributes, "val")
                && !value.trim().is_empty()
            {
                return value.trim().to_owned();
            }
        }
    } else {
        let expected_parent = if dictionary == Dictionary::Stdict {
            "word_info"
        } else {
            "wordInfo"
        };
        let mut stack: Vec<&str> = Vec::new();
        for record in records {
            match record {
                SourceRecord::StartElement { name, .. } => stack.push(local_name(name)),
                SourceRecord::ElementText { value, .. }
                    if stack.last() == Some(&"word")
                        && stack.iter().rev().nth(1) == Some(&expected_parent)
                        && !value.trim().is_empty() =>
                {
                    return value.trim().to_owned();
                }
                SourceRecord::EndElement { .. } => {
                    stack.pop();
                }
                SourceRecord::EmptyElement { .. }
                | SourceRecord::ElementText { .. }
                | SourceRecord::TailText { .. } => {}
            }
        }
    }
    format!("항목 {index}")
}

fn attribute_value<'a>(attributes: &'a [SourceAttribute], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|attribute| local_name(&attribute.name) == name)
        .map(|attribute| attribute.value.as_str())
}

fn title_xhtml(
    title: &str,
    volume: &Volume,
    entries: u64,
    first_headword: &str,
    last_headword: &str,
    digest: &DigestSummary,
) -> String {
    let source_name = volume.relative_source.to_string_lossy().replace('\\', "/");
    let body = format!(
        "<section epub:type=\"titlepage\">\n<h1>{}</h1>\n\
         <dl class=\"book-summary\">\
         <dt>사전</dt><dd>{}</dd>\
         <dt>권</dt><dd>{}/{}</dd>\
         <dt>원본 XML</dt><dd>{}</dd>\
         <dt>항목 수</dt><dd>{entries}</dd>\
         <dt>표제어 범위</dt><dd>{} — {}</dd>\
         <dt>레코드 digest</dt><dd>{}: {}</dd>\
         </dl>\n</section>",
        escape_xml(title),
        escape_xml(volume.dictionary.series()),
        volume.number,
        volume.total,
        escape_xml(&source_name),
        render_value(first_headword),
        render_value(last_headword),
        escape_xml(digest.schema),
        escape_xml(&digest.sha256)
    );
    xhtml_document(title, &body, "../styles/book.css")
}

fn nav_xhtml(title: &str, chapters: &[ChapterInfo]) -> String {
    let mut items = String::from("<li><a href=\"text/title.xhtml\">문서 정보</a></li>\n");
    for (index, chapter) in chapters.iter().enumerate() {
        let label = match (chapter.first_entry, chapter.last_entry) {
            (Some(first), Some(last)) => {
                format!(
                    "{}장 · {first}–{last}항목 ({}개)",
                    index + 1,
                    chapter.entries
                )
            }
            _ => format!("{}장 · 문서 레코드 {}개", index + 1, chapter.records),
        };
        items.push_str(&format!(
            "<li><a href=\"text/{}\">{}</a></li>\n",
            escape_xml(&chapter.filename),
            escape_xml(&label)
        ));
    }
    let first_chapter = &chapters[0].filename;
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"{XHTML_NS}\" xmlns:epub=\"{EPUB_NS}\" \
         lang=\"ko\" xml:lang=\"ko\">\n<head>\n<meta charset=\"UTF-8\" />\n\
         <title>{} 목차</title>\n\
         <link rel=\"stylesheet\" type=\"text/css\" href=\"styles/book.css\" />\n\
         </head>\n<body>\n<nav epub:type=\"toc\" id=\"toc\">\n<h1>{}</h1>\n\
         <ol>\n{items}</ol>\n</nav>\n\
         <nav epub:type=\"landmarks\" hidden=\"hidden\"><h2>안내</h2><ol>\
         <li><a epub:type=\"titlepage\" href=\"text/title.xhtml\">표제</a></li>\
         <li><a epub:type=\"bodymatter\" href=\"text/{}\">본문</a></li>\
         </ol></nav>\n</body>\n</html>\n",
        escape_xml(title),
        escape_xml(title),
        escape_xml(first_chapter)
    )
}

fn package_opf(
    title: &str,
    volume: &Volume,
    source_name: &str,
    identifier: &str,
    chapters: &[ChapterInfo],
) -> String {
    let mut manifest = String::from(
        "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\" />\n\
         <item id=\"css\" href=\"styles/book.css\" media-type=\"text/css\" />\n\
         <item id=\"title\" href=\"text/title.xhtml\" media-type=\"application/xhtml+xml\" />\n",
    );
    let mut spine = String::from("<itemref idref=\"title\" />\n");
    for (index, chapter) in chapters.iter().enumerate() {
        let id = format!("chapter-{:04}", index + 1);
        manifest.push_str(&format!(
            "<item id=\"{id}\" href=\"text/{}\" media-type=\"application/xhtml+xml\" />\n",
            escape_xml(&chapter.filename)
        ));
        spine.push_str(&format!("<itemref idref=\"{id}\" />\n"));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <package xmlns=\"{OPF_NS}\" xmlns:dc=\"{DC_NS}\" version=\"3.0\" \
         unique-identifier=\"book-id\" xml:lang=\"ko\">\n<metadata>\n\
         <dc:identifier id=\"book-id\">{}</dc:identifier>\n\
         <dc:title>{}</dc:title>\n<dc:language>ko</dc:language>\n\
         <dc:creator>국립국어원</dc:creator>\n<dc:source>{}</dc:source>\n\
         <meta property=\"dcterms:modified\">{MODIFIED_TIMESTAMP}</meta>\n\
         <meta property=\"belongs-to-collection\" id=\"collection\">{}</meta>\n\
         <meta refines=\"#collection\" property=\"collection-type\">series</meta>\n\
         <meta refines=\"#collection\" property=\"group-position\">{}</meta>\n\
         </metadata>\n<manifest>\n{manifest}</manifest>\n\
         <spine page-progression-direction=\"ltr\">\n{spine}</spine>\n</package>\n",
        escape_xml(identifier),
        escape_xml(title),
        escape_xml(source_name),
        escape_xml(volume.dictionary.series()),
        volume.number
    )
}

fn container_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <container version=\"1.0\" xmlns=\"{CONTAINER_NS}\">\n<rootfiles>\
         <rootfile full-path=\"EPUB/package.opf\" \
         media-type=\"application/oebps-package+xml\" />\
         </rootfiles>\n</container>\n"
    )
}

fn package_members(chapters: &[ChapterInfo]) -> Vec<String> {
    let mut names = vec![
        "mimetype".to_owned(),
        "META-INF/container.xml".to_owned(),
        "EPUB/package.opf".to_owned(),
        "EPUB/nav.xhtml".to_owned(),
        "EPUB/styles/book.css".to_owned(),
        "EPUB/text/title.xhtml".to_owned(),
    ];
    names.extend(
        chapters
            .iter()
            .map(|chapter| format!("EPUB/text/{}", chapter.filename)),
    );
    names
}

fn package_stage(
    stage: &Path,
    output_path: &Path,
    expected_members: &[String],
) -> Result<AtomicWriteFile, EpubError> {
    let atomic = AtomicWriteFile::options().read(true).open(output_path)?;
    let mut archive = ZipWriter::new(atomic);
    let stored = SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);
    archive.start_file("mimetype", stored)?;
    archive.write_all(b"application/epub+zip")?;

    let compressed = SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(6))
        .unix_permissions(0o644);
    for name in &expected_members[1..] {
        archive.start_file(name, compressed)?;
        let mut source = File::open(stage.join(name))?;
        io::copy(&mut source, &mut archive)?;
    }

    let mut atomic = archive.finish()?;
    atomic.flush()?;
    atomic.as_file().sync_all()?;
    validate_package(&mut atomic, expected_members)?;
    Ok(atomic)
}

fn validate_package(
    file: &mut AtomicWriteFile,
    expected_members: &[String],
) -> Result<(), EpubError> {
    file.seek(SeekFrom::Start(0))?;
    let mut archive = ZipArchive::new(&mut *file)?;
    let mut names = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        names.push(archive.by_index(index)?.name().to_owned());
    }
    if names != expected_members {
        return Err(EpubError::PackageMismatch(format!(
            "ZIP members differ: expected {expected_members:?}, got {names:?}"
        )));
    }

    let mut mimetype = archive.by_index(0)?;
    if mimetype.compression() != CompressionMethod::Stored {
        return Err(EpubError::PackageMismatch(
            "mimetype is compressed".to_owned(),
        ));
    }
    let mut value = Vec::new();
    mimetype.read_to_end(&mut value)?;
    if value != b"application/epub+zip" {
        return Err(EpubError::PackageMismatch(
            "mimetype has an invalid value".to_owned(),
        ));
    }
    Ok(())
}

fn total_records(digest: &DigestSummary) -> u64 {
    digest.counts.elements
        + digest.counts.end_elements
        + digest.counts.element_texts
        + digest.counts.tail_texts
}

#[cfg(test)]
mod tests {
    use crate::catalog::Dictionary;
    use crate::record::{SourceAttribute, SourceRecord};

    use super::extract_headword;

    #[test]
    fn extracts_headwords_without_controlling_record_inclusion() {
        let krdict = vec![SourceRecord::EmptyElement {
            depth: 2,
            name: "feat".to_owned(),
            attributes: vec![
                SourceAttribute {
                    name: "att".to_owned(),
                    value: "writtenForm".to_owned(),
                },
                SourceAttribute {
                    name: "val".to_owned(),
                    value: "가상어".to_owned(),
                },
            ],
        }];
        let stdict = vec![
            SourceRecord::StartElement {
                depth: 0,
                name: "word_info".to_owned(),
                attributes: vec![],
            },
            SourceRecord::StartElement {
                depth: 1,
                name: "word".to_owned(),
                attributes: vec![],
            },
            SourceRecord::ElementText {
                depth: 1,
                value: "합성어".to_owned(),
            },
            SourceRecord::EndElement {
                depth: 1,
                name: "word".to_owned(),
            },
        ];

        assert_eq!(extract_headword(Dictionary::Krdict, &krdict, 1), "가상어");
        assert_eq!(extract_headword(Dictionary::Stdict, &stdict, 1), "합성어");
    }
}
