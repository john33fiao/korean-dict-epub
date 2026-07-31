use std::error::Error;
use std::fmt;

use serde::Serialize;

use crate::catalog::Dictionary;

pub const CANONICAL_ID_SCHEMA: &str = "kweb-canonical-id-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Entry,
    PartOfSpeech,
    CommonPattern,
    Sense,
}

impl EntityKind {
    pub const ALL: [Self; 4] = [
        Self::Entry,
        Self::PartOfSpeech,
        Self::CommonPattern,
        Self::Sense,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::PartOfSpeech => "part_of_speech",
            Self::CommonPattern => "common_pattern",
            Self::Sense => "sense",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationStatus {
    Resolved,
    SelfReference,
    Unresolved,
    Ambiguous,
}

impl RelationStatus {
    pub const ALL: [Self; 4] = [
        Self::Resolved,
        Self::SelfReference,
        Self::Unresolved,
        Self::Ambiguous,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::SelfReference => "self_reference",
            Self::Unresolved => "unresolved",
            Self::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CanonicalIdParts<'a> {
    pub corpus_commit: &'a str,
    pub dictionary: Dictionary,
    pub entity_kind: EntityKind,
    pub native_key: Option<&'a str>,
    pub owning_entry_id: Option<&'a str>,
    pub source_locator: &'a str,
    pub namespace_occurrences: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalIdError {
    EmptyCorpusCommit,
    EmptySourceLocator,
    EntryHasOwner,
    NestedEntityMissingOwner,
}

impl fmt::Display for CanonicalIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCorpusCommit => write!(formatter, "corpus commit must not be empty"),
            Self::EmptySourceLocator => write!(formatter, "source locator must not be empty"),
            Self::EntryHasOwner => write!(formatter, "entry canonical IDs must not have an owner"),
            Self::NestedEntityMissingOwner => {
                write!(
                    formatter,
                    "nested entity canonical IDs require an owning entry"
                )
            }
        }
    }
}

impl Error for CanonicalIdError {}

pub fn canonical_id(parts: CanonicalIdParts<'_>) -> Result<String, CanonicalIdError> {
    let corpus_commit = parts.corpus_commit.trim();
    if corpus_commit.is_empty() {
        return Err(CanonicalIdError::EmptyCorpusCommit);
    }
    if parts.source_locator.is_empty() {
        return Err(CanonicalIdError::EmptySourceLocator);
    }

    let native_key = parts
        .native_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(percent_encode)
        .unwrap_or_else(|| "missing".to_owned());

    if parts.entity_kind == EntityKind::Entry {
        if parts.owning_entry_id.is_some() {
            return Err(CanonicalIdError::EntryHasOwner);
        }
        let mut value = format!(
            "kweb:v1/{}/{}/entry/{native_key}",
            percent_encode(corpus_commit),
            parts.dictionary.key()
        );
        append_disambiguator(
            &mut value,
            parts.namespace_occurrences,
            parts.source_locator,
        );
        return Ok(value);
    }

    let owner = parts
        .owning_entry_id
        .filter(|value| !value.is_empty())
        .ok_or(CanonicalIdError::NestedEntityMissingOwner)?;
    let mut value = format!("{owner}/{}/{native_key}", parts.entity_kind.key());
    append_disambiguator(
        &mut value,
        parts.namespace_occurrences,
        parts.source_locator,
    );
    Ok(value)
}

fn append_disambiguator(value: &mut String, occurrences: u32, source_locator: &str) {
    if occurrences != 1 {
        value.push_str("/at/");
        value.push_str(&percent_encode(source_locator));
    }
}

fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(output, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{CanonicalIdError, CanonicalIdParts, EntityKind, canonical_id};
    use crate::catalog::Dictionary;

    #[test]
    fn preserves_the_existing_entry_and_nested_id_contract() {
        let entry = canonical_id(CanonicalIdParts {
            corpus_commit: "abc123",
            dictionary: Dictionary::Krdict,
            entity_kind: EntityKind::Entry,
            native_key: Some("001"),
            owning_entry_id: None,
            source_locator: "krdict:krdict/001.xml#entry=1",
            namespace_occurrences: 2,
        })
        .unwrap();
        assert_eq!(
            entry,
            "kweb:v1/abc123/krdict/entry/001/at/krdict%3Akrdict%2F001.xml%23entry%3D1"
        );

        let sense = canonical_id(CanonicalIdParts {
            corpus_commit: "abc123",
            dictionary: Dictionary::Krdict,
            entity_kind: EntityKind::Sense,
            native_key: Some("1"),
            owning_entry_id: Some(&entry),
            source_locator: "krdict:krdict/001.xml#entry=1/sense=1",
            namespace_occurrences: 1,
        })
        .unwrap();
        assert_eq!(sense, format!("{entry}/sense/1"));
    }

    #[test]
    fn rejects_nested_ids_without_an_owner() {
        let error = canonical_id(CanonicalIdParts {
            corpus_commit: "abc123",
            dictionary: Dictionary::Opendict,
            entity_kind: EntityKind::Sense,
            native_key: Some("1"),
            owning_entry_id: None,
            source_locator: "opendict:opendict/001.xml#entry=1/sense=1",
            namespace_occurrences: 1,
        })
        .unwrap_err();
        assert_eq!(error, CanonicalIdError::NestedEntityMissingOwner);
    }
}
