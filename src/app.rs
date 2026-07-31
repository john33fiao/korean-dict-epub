use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::cli::{Cli, Command, DictionarySelection, PreflightArgs};

pub const RUNTIME_ERROR_EXIT_CODE: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightPlan {
    source: PathBuf,
    output: PathBuf,
    dictionary: DictionarySelection,
    jobs: usize,
    overwrite: bool,
    keep_going: bool,
}

impl fmt::Display for PreflightPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "status=ready")?;
        writeln!(formatter, "source={}", self.source.display())?;
        writeln!(formatter, "output={}", self.output.display())?;
        writeln!(formatter, "dictionary={}", self.dictionary.as_str())?;
        writeln!(formatter, "jobs={}", self.jobs)?;
        writeln!(formatter, "overwrite={}", self.overwrite)?;
        write!(formatter, "keep_going={}", self.keep_going)
    }
}

#[derive(Debug)]
pub enum AppError {
    InvalidSource {
        path: PathBuf,
        reason: String,
    },
    MissingDictionaryDirectory {
        path: PathBuf,
        dictionary: &'static str,
    },
    InvalidOutput {
        path: PathBuf,
        reason: String,
    },
    UnsafeOutput {
        source: PathBuf,
        output: PathBuf,
    },
}

impl AppError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidSource { .. } => "KDEP-E001",
            Self::MissingDictionaryDirectory { .. } => "KDEP-E002",
            Self::InvalidOutput { .. } => "KDEP-E003",
            Self::UnsafeOutput { .. } => "KDEP-E004",
        }
    }

    pub const fn exit_code(&self) -> u8 {
        RUNTIME_ERROR_EXIT_CODE
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource { path, reason } => {
                write!(
                    formatter,
                    "invalid source directory '{}': {reason}",
                    path.display()
                )
            }
            Self::MissingDictionaryDirectory { path, dictionary } => {
                write!(
                    formatter,
                    "source is missing the {dictionary} directory '{}'",
                    path.display()
                )
            }
            Self::InvalidOutput { path, reason } => {
                write!(
                    formatter,
                    "invalid output directory '{}': {reason}",
                    path.display()
                )
            }
            Self::UnsafeOutput { source, output } => {
                write!(
                    formatter,
                    "output '{}' must not contain or be contained by source '{}'",
                    output.display(),
                    source.display()
                )
            }
        }
    }
}

impl Error for AppError {}

pub fn run(cli: Cli) -> Result<PreflightPlan, AppError> {
    match cli.command {
        Command::Preflight(args) => preflight(args),
    }
}

fn preflight(args: PreflightArgs) -> Result<PreflightPlan, AppError> {
    let source = resolve_source(&args.source)?;
    validate_dictionary_directories(&source, args.dictionary)?;
    let output = resolve_output(&args.output)?;
    validate_output_boundary(&source, &output)?;

    Ok(PreflightPlan {
        source,
        output,
        dictionary: args.dictionary,
        jobs: args.jobs.get(),
        overwrite: args.overwrite,
        keep_going: args.keep_going,
    })
}

fn resolve_source(path: &Path) -> Result<PathBuf, AppError> {
    let absolute = absolute_clean(path).map_err(|reason| AppError::InvalidSource {
        path: path.to_path_buf(),
        reason,
    })?;
    let canonical = fs::canonicalize(&absolute).map_err(|error| AppError::InvalidSource {
        path: absolute.clone(),
        reason: error.to_string(),
    })?;
    if !canonical.is_dir() {
        return Err(AppError::InvalidSource {
            path: canonical,
            reason: "not a directory".to_owned(),
        });
    }
    Ok(canonical)
}

fn validate_dictionary_directories(
    source: &Path,
    selection: DictionarySelection,
) -> Result<(), AppError> {
    for dictionary in selection.directories() {
        let path = source.join(dictionary);
        if !path.is_dir() {
            return Err(AppError::MissingDictionaryDirectory { path, dictionary });
        }
    }
    Ok(())
}

fn resolve_output(path: &Path) -> Result<PathBuf, AppError> {
    let absolute = absolute_clean(path).map_err(|reason| AppError::InvalidOutput {
        path: path.to_path_buf(),
        reason,
    })?;
    if absolute.exists() {
        let canonical = fs::canonicalize(&absolute).map_err(|error| AppError::InvalidOutput {
            path: absolute.clone(),
            reason: error.to_string(),
        })?;
        if !canonical.is_dir() {
            return Err(AppError::InvalidOutput {
                path: canonical,
                reason: "existing path is not a directory".to_owned(),
            });
        }
        return Ok(canonical);
    }

    canonicalize_with_missing_tail(&absolute).map_err(|reason| AppError::InvalidOutput {
        path: absolute,
        reason,
    })
}

fn validate_output_boundary(source: &Path, output: &Path) -> Result<(), AppError> {
    if source == output || source.starts_with(output) || output.starts_with(source) {
        return Err(AppError::UnsafeOutput {
            source: source.to_path_buf(),
            output: output.to_path_buf(),
        });
    }
    Ok(())
}

fn absolute_clean(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    Ok(clean_path(&absolute))
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
    use std::fs;
    use std::num::NonZeroUsize;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::cli::{DictionarySelection, PreflightArgs};

    use super::{AppError, preflight};

    static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    struct TempFixture {
        root: PathBuf,
    }

    impl TempFixture {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "korean-dict-epub-{}-{sequence}",
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
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn args(source: PathBuf, output: PathBuf) -> PreflightArgs {
        PreflightArgs {
            source,
            output,
            dictionary: DictionarySelection::All,
            jobs: NonZeroUsize::new(1).expect("one is non-zero"),
            overwrite: false,
            keep_going: false,
        }
    }

    #[test]
    fn accepts_complete_fixture_without_creating_output() {
        let fixture = TempFixture::new();
        let source = fixture.source_with_all_dictionaries();
        let output = fixture.path().join("output");

        let plan = preflight(args(source, output.clone())).expect("preflight should pass");

        assert!(plan.to_string().contains("status=ready"));
        assert!(!output.exists());
    }

    #[test]
    fn selected_dictionary_directory_is_required() {
        let fixture = TempFixture::new();
        let source = fixture.path().join("source");
        fs::create_dir_all(&source).expect("source fixture should be created");
        let mut arguments = args(source, fixture.path().join("output"));
        arguments.dictionary = DictionarySelection::Krdict;

        let error = preflight(arguments).expect_err("missing krdict directory must fail");

        assert!(matches!(
            error,
            AppError::MissingDictionaryDirectory {
                dictionary: "krdict",
                ..
            }
        ));
    }

    #[test]
    fn output_inside_source_is_rejected() {
        let fixture = TempFixture::new();
        let source = fixture.source_with_all_dictionaries();
        let output = source.join("generated");

        let error = preflight(args(source, output)).expect_err("unsafe output must fail");

        assert!(matches!(error, AppError::UnsafeOutput { .. }));
    }

    #[test]
    fn source_inside_output_is_rejected() {
        let fixture = TempFixture::new();
        let output = fixture.path().to_path_buf();
        let source = fixture.source_with_all_dictionaries();

        let error = preflight(args(source, output)).expect_err("unsafe output must fail");

        assert!(matches!(error, AppError::UnsafeOutput { .. }));
    }
}
