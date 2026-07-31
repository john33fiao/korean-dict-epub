use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::str;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dictionary {
    Krdict,
    Stdict,
    Opendict,
}

impl Dictionary {
    pub const ALL: [Self; 3] = [Self::Krdict, Self::Stdict, Self::Opendict];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Krdict => "krdict",
            Self::Stdict => "stdict",
            Self::Opendict => "opendict",
        }
    }

    pub const fn series(self) -> &'static str {
        match self {
            Self::Krdict => "한국어기초사전",
            Self::Stdict => "표준국어대사전",
            Self::Opendict => "우리말샘",
        }
    }

    pub const fn filename_prefix(self) -> &'static str {
        match self {
            Self::Krdict => "01-한국어기초사전",
            Self::Stdict => "02-표준국어대사전",
            Self::Opendict => "03-우리말샘",
        }
    }

    pub const fn entry_element(self) -> &'static str {
        match self {
            Self::Krdict => "LexicalEntry",
            Self::Stdict | Self::Opendict => "item",
        }
    }

    pub fn is_entry_element(self, qualified_name: &str) -> bool {
        local_name(qualified_name) == self.entry_element()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub dictionary: Dictionary,
    pub number: usize,
    pub total: usize,
    pub relative_source: PathBuf,
    pub source: PathBuf,
    pub output_filename: String,
}

#[derive(Debug)]
pub enum CatalogError {
    InvalidSource(PathBuf, std::io::Error),
    GitUnavailable(std::io::Error),
    GitFailed(String),
    InvalidGitOutput(str::Utf8Error),
    UnsafeTrackedPath(String),
    MissingTrackedFile(PathBuf),
    SymlinkInput(PathBuf),
    MissingDictionary(&'static str),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource(path, error) => {
                write!(
                    formatter,
                    "could not resolve source directory '{}': {error}",
                    path.display()
                )
            }
            Self::GitUnavailable(error) => {
                write!(
                    formatter,
                    "could not start Git to discover tracked XML: {error}"
                )
            }
            Self::GitFailed(message) => {
                write!(formatter, "Git could not list tracked XML: {message}")
            }
            Self::InvalidGitOutput(error) => {
                write!(formatter, "Git returned a non-UTF-8 tracked path: {error}")
            }
            Self::UnsafeTrackedPath(path) => {
                write!(
                    formatter,
                    "tracked XML path is outside the allowed layout: {path}"
                )
            }
            Self::MissingTrackedFile(path) => {
                write!(
                    formatter,
                    "tracked XML is missing or is not a file: {}",
                    path.display()
                )
            }
            Self::SymlinkInput(path) => {
                write!(
                    formatter,
                    "tracked XML must not be a symbolic link: {}",
                    path.display()
                )
            }
            Self::MissingDictionary(dictionary) => {
                write!(formatter, "no tracked XML was found for {dictionary}")
            }
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSource(_, error) => Some(error),
            Self::GitUnavailable(error) => Some(error),
            Self::InvalidGitOutput(error) => Some(error),
            Self::GitFailed(_)
            | Self::UnsafeTrackedPath(_)
            | Self::MissingTrackedFile(_)
            | Self::SymlinkInput(_)
            | Self::MissingDictionary(_) => None,
        }
    }
}

pub fn discover(source: &Path, dictionaries: &[Dictionary]) -> Result<Vec<Volume>, CatalogError> {
    let source = fs::canonicalize(source)
        .map_err(|error| CatalogError::InvalidSource(source.to_path_buf(), error))?;
    let tracked = git_tracked_xml(&source, dictionaries)?;
    let mut volumes = Vec::new();

    for dictionary in dictionaries {
        let mut files: Vec<PathBuf> = tracked
            .iter()
            .filter(|path| path.starts_with(dictionary.key()))
            .cloned()
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(CatalogError::MissingDictionary(dictionary.key()));
        }

        let total = files.len();
        for (index, relative_source) in files.into_iter().enumerate() {
            let path = source.join(&relative_source);
            validate_tracked_file(&source, &path)?;
            let number = index + 1;
            volumes.push(Volume {
                dictionary: *dictionary,
                number,
                total,
                relative_source,
                source: path,
                output_filename: format!(
                    "{}-{number:03}-of-{total:03}.epub",
                    dictionary.filename_prefix()
                ),
            });
        }
    }

    Ok(volumes)
}

fn git_tracked_xml(
    source: &Path,
    dictionaries: &[Dictionary],
) -> Result<Vec<PathBuf>, CatalogError> {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-c")
        .arg(format!("safe.directory={}", git_safe_directory(source)))
        .arg("-C")
        .arg(source)
        .arg("ls-files")
        .arg("-z")
        .arg("--");
    for dictionary in dictionaries {
        command.arg(format!("{}/*.xml", dictionary.key()));
    }

    let output = command.output().map_err(CatalogError::GitUnavailable)?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CatalogError::GitFailed(if message.is_empty() {
            format!("process exited with {}", output.status)
        } else {
            message
        }));
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = str::from_utf8(path).map_err(CatalogError::InvalidGitOutput)?;
            validate_relative_layout(path, dictionaries)
        })
        .collect()
}

fn git_safe_directory(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return format!("//{}", unc.replace('\\', "/"));
        }
        value
            .strip_prefix(r"\\?\")
            .unwrap_or(&value)
            .replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        value.into_owned()
    }
}

fn validate_relative_layout(
    value: &str,
    dictionaries: &[Dictionary],
) -> Result<PathBuf, CatalogError> {
    let path = Path::new(value);
    let components: Vec<_> = path.components().collect();
    let valid_components = matches!(
        components.as_slice(),
        [Component::Normal(_), Component::Normal(_)]
    );
    let valid_dictionary = dictionaries
        .iter()
        .any(|dictionary| path.starts_with(dictionary.key()));
    let valid_extension = path.extension().is_some_and(|extension| extension == "xml");
    if !valid_components || !valid_dictionary || !valid_extension {
        return Err(CatalogError::UnsafeTrackedPath(value.to_owned()));
    }
    Ok(path.to_path_buf())
}

fn validate_tracked_file(source: &Path, path: &Path) -> Result<(), CatalogError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| CatalogError::MissingTrackedFile(path.to_path_buf()))?;
    if metadata.file_type().is_symlink() {
        return Err(CatalogError::SymlinkInput(path.to_path_buf()));
    }
    if !metadata.is_file() {
        return Err(CatalogError::MissingTrackedFile(path.to_path_buf()));
    }
    let canonical =
        fs::canonicalize(path).map_err(|_| CatalogError::MissingTrackedFile(path.to_path_buf()))?;
    if !canonical.starts_with(source) {
        return Err(CatalogError::UnsafeTrackedPath(
            path.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}

fn local_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit_once(':')
        .map_or(qualified_name, |(_, local)| local)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(windows)]
    use super::git_safe_directory;
    use super::{Dictionary, discover};

    static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    struct GitFixture {
        root: PathBuf,
    }

    impl GitFixture {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "korean-dict-epub-catalog-{}-{sequence}",
                std::process::id()
            ));
            if root.exists() {
                fs::remove_dir_all(&root).expect("stale fixture should be removable");
            }
            fs::create_dir_all(root.join("krdict")).expect("fixture directory should be created");
            let status = Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(&root)
                .status()
                .expect("Git should start for the catalog fixture");
            assert!(status.success(), "Git fixture should initialize");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, relative: &str) {
            fs::write(self.root.join(relative), "<root/>").expect("fixture XML should be written");
        }

        fn track(&self, relative: &str) {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .arg("add")
                .arg("--")
                .arg(relative)
                .status()
                .expect("Git should add the fixture");
            assert!(status.success(), "fixture should be tracked");
        }
    }

    impl Drop for GitFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn discovers_only_tracked_xml_in_filename_order() {
        let fixture = GitFixture::new();
        fixture.write("krdict/002.xml");
        fixture.write("krdict/001.xml");
        fixture.write("krdict/999.xml");
        fixture.track("krdict/002.xml");
        fixture.track("krdict/001.xml");

        let catalog = discover(fixture.path(), &[Dictionary::Krdict]).expect("catalog should load");

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].relative_source, Path::new("krdict/001.xml"));
        assert_eq!(catalog[0].number, 1);
        assert_eq!(catalog[0].total, 2);
        assert_eq!(
            catalog[0].output_filename,
            "01-한국어기초사전-001-of-002.epub"
        );
        assert_eq!(catalog[1].relative_source, Path::new("krdict/002.xml"));
    }

    #[test]
    fn entry_boundaries_use_local_name_without_filtering_unknown_fields() {
        assert!(Dictionary::Krdict.is_entry_element("LexicalEntry"));
        assert!(Dictionary::Krdict.is_entry_element("nikl:LexicalEntry"));
        assert!(Dictionary::Stdict.is_entry_element("item"));
        assert!(!Dictionary::Opendict.is_entry_element("future:item-extra"));
    }

    #[cfg(windows)]
    #[test]
    fn canonical_windows_path_matches_git_safe_directory_format() {
        assert_eq!(
            git_safe_directory(Path::new(r"\\?\C:\Cloud\dictionary")),
            "C:/Cloud/dictionary"
        );
        assert_eq!(
            git_safe_directory(Path::new(r"\\?\UNC\server\share\dictionary")),
            "//server/share/dictionary"
        );
    }
}
