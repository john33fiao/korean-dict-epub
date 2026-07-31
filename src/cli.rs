use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::epub::{DEFAULT_CHAPTER_BYTES, DEFAULT_ENTRIES_PER_CHAPTER};

pub const DEFAULT_SOURCE: &str = "references/korean-dict-nikl";
pub const DEFAULT_OUTPUT: &str = "outputs/rust";

#[derive(Debug, Parser)]
#[command(
    name = "korean-dict-epub",
    version,
    about = "Convert NIKL dictionary XML files into sequential-reading EPUB 3 books",
    long_about = "Rust implementation of the NIKL XML-to-EPUB converter.\n\
                  Preflight validates paths and policy; inspect reports one tracked XML digest; \
                  build creates one tracked volume as an EPUB 3 book.",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate paths and execution policy without reading XML or writing output
    Preflight(PreflightArgs),

    /// Read one tracked XML volume and report its lossless record digest
    Inspect(InspectArgs),

    /// Build one tracked XML volume as an EPUB 3 book
    Build(BuildArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PreflightArgs {
    /// Root directory containing krdict, stdict, and opendict inputs
    #[arg(long, value_name = "PATH", default_value = DEFAULT_SOURCE)]
    pub source: PathBuf,

    /// Local directory where future build output will be written
    #[arg(long, value_name = "PATH", default_value = DEFAULT_OUTPUT)]
    pub output: PathBuf,

    /// Dictionary set to process
    #[arg(long, value_enum, default_value_t = DictionarySelection::All)]
    pub dictionary: DictionarySelection,

    /// Maximum file-level worker count
    #[arg(long, value_name = "COUNT", default_value = "1")]
    pub jobs: NonZeroUsize,

    /// Permit replacement of existing generated files in future build commands
    #[arg(long)]
    pub overwrite: bool,

    /// Continue processing other volumes after a failure in future build commands
    #[arg(long)]
    pub keep_going: bool,
}

#[derive(Debug, Clone, Args)]
pub struct InspectArgs {
    /// Root directory containing the Git-tracked dictionary inputs
    #[arg(long, value_name = "PATH", default_value = DEFAULT_SOURCE)]
    pub source: PathBuf,

    /// Dictionary containing the volume
    #[arg(long, value_enum)]
    pub dictionary: DictionaryName,

    /// One-based volume number within the selected dictionary
    #[arg(long, value_name = "NUMBER", default_value = "1")]
    pub volume: NonZeroUsize,
}

#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    /// Root directory containing the Git-tracked dictionary inputs
    #[arg(long, value_name = "PATH", default_value = DEFAULT_SOURCE)]
    pub source: PathBuf,

    /// Local directory where the EPUB will be written
    #[arg(long, value_name = "PATH", default_value = DEFAULT_OUTPUT)]
    pub output: PathBuf,

    /// Dictionary containing the volume
    #[arg(long, value_enum)]
    pub dictionary: DictionaryName,

    /// One-based volume number within the selected dictionary
    #[arg(long, value_name = "NUMBER", default_value = "1")]
    pub volume: NonZeroUsize,

    /// Maximum complete entries per XHTML chapter
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = NonZeroUsize::new(DEFAULT_ENTRIES_PER_CHAPTER).expect("default is non-zero")
    )]
    pub entries_per_chapter: NonZeroUsize,

    /// Target maximum serialized bytes per XHTML chapter
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = NonZeroUsize::new(DEFAULT_CHAPTER_BYTES).expect("default is non-zero")
    )]
    pub chapter_bytes: NonZeroUsize,

    /// Atomically replace an existing EPUB with the same filename
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DictionaryName {
    Krdict,
    Stdict,
    Opendict,
}

impl From<DictionaryName> for crate::catalog::Dictionary {
    fn from(value: DictionaryName) -> Self {
        match value {
            DictionaryName::Krdict => Self::Krdict,
            DictionaryName::Stdict => Self::Stdict,
            DictionaryName::Opendict => Self::Opendict,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DictionarySelection {
    All,
    Krdict,
    Stdict,
    Opendict,
}

impl DictionarySelection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Krdict => "krdict",
            Self::Stdict => "stdict",
            Self::Opendict => "opendict",
        }
    }

    pub const fn directories(self) -> &'static [&'static str] {
        match self {
            Self::All => &["krdict", "stdict", "opendict"],
            Self::Krdict => &["krdict"],
            Self::Stdict => &["stdict"],
            Self::Opendict => &["opendict"],
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{
        Cli, Command, DEFAULT_CHAPTER_BYTES, DEFAULT_ENTRIES_PER_CHAPTER, DEFAULT_OUTPUT,
        DEFAULT_SOURCE, DictionaryName, DictionarySelection,
    };

    #[test]
    fn preflight_defaults_are_safe() {
        let cli = Cli::try_parse_from(["korean-dict-epub", "preflight"])
            .expect("default preflight arguments should parse");
        let Command::Preflight(args) = cli.command else {
            panic!("preflight command should be selected")
        };

        assert_eq!(args.source.to_string_lossy(), DEFAULT_SOURCE);
        assert_eq!(args.output.to_string_lossy(), DEFAULT_OUTPUT);
        assert_eq!(args.dictionary, DictionarySelection::All);
        assert_eq!(args.jobs.get(), 1);
        assert!(!args.overwrite);
        assert!(!args.keep_going);
    }

    #[test]
    fn zero_jobs_is_rejected() {
        let error = Cli::try_parse_from(["korean-dict-epub", "preflight", "--jobs", "0"])
            .expect_err("zero workers must be rejected");

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn unknown_dictionary_is_rejected() {
        let error =
            Cli::try_parse_from(["korean-dict-epub", "preflight", "--dictionary", "unknown"])
                .expect_err("unknown dictionary must be rejected");

        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn inspect_requires_one_dictionary_and_defaults_to_first_volume() {
        let cli = Cli::try_parse_from(["korean-dict-epub", "inspect", "--dictionary", "stdict"])
            .expect("inspect arguments should parse");
        let Command::Inspect(args) = cli.command else {
            panic!("inspect command should be selected")
        };

        assert_eq!(args.source.to_string_lossy(), DEFAULT_SOURCE);
        assert_eq!(args.dictionary, DictionaryName::Stdict);
        assert_eq!(args.volume.get(), 1);
    }

    #[test]
    fn inspect_rejects_all_dictionary_selection() {
        let error = Cli::try_parse_from(["korean-dict-epub", "inspect", "--dictionary", "all"])
            .expect_err("inspect must select one dictionary");

        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn build_defaults_keep_output_and_chapter_limits_safe() {
        let cli = Cli::try_parse_from(["korean-dict-epub", "build", "--dictionary", "krdict"])
            .expect("build arguments should parse");
        let Command::Build(args) = cli.command else {
            panic!("build command should be selected")
        };

        assert_eq!(args.source.to_string_lossy(), DEFAULT_SOURCE);
        assert_eq!(args.output.to_string_lossy(), DEFAULT_OUTPUT);
        assert_eq!(args.volume.get(), 1);
        assert_eq!(args.entries_per_chapter.get(), DEFAULT_ENTRIES_PER_CHAPTER);
        assert_eq!(args.chapter_bytes.get(), DEFAULT_CHAPTER_BYTES);
        assert!(!args.overwrite);
    }
}
