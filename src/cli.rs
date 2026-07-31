use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub const DEFAULT_SOURCE: &str = "references/korean-dict-nikl";
pub const DEFAULT_OUTPUT: &str = "outputs/rust";

#[derive(Debug, Parser)]
#[command(
    name = "korean-dict-epub",
    version,
    about = "Convert NIKL dictionary XML files into sequential-reading EPUB 3 books",
    long_about = "Rust implementation of the NIKL XML-to-EPUB converter.\n\
                  KDEP-001 provides the preflight contract only; XML conversion is not implemented yet.",
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

    use super::{Cli, Command, DEFAULT_OUTPUT, DEFAULT_SOURCE, DictionarySelection};

    #[test]
    fn preflight_defaults_are_safe() {
        let cli = Cli::try_parse_from(["korean-dict-epub", "preflight"])
            .expect("default preflight arguments should parse");
        let Command::Preflight(args) = cli.command;

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
}
