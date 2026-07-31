use std::fmt::Write as _;

use sha2::{Digest, Sha256};

pub const DIGEST_SCHEMA: &str = "kdep-source-record-v1";
const DIGEST_PREAMBLE: &[u8] = b"korean-dict-epub/source-record-digest/v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAttribute {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRecord {
    StartElement {
        depth: usize,
        name: String,
        attributes: Vec<SourceAttribute>,
    },
    EmptyElement {
        depth: usize,
        name: String,
        attributes: Vec<SourceAttribute>,
    },
    ElementText {
        depth: usize,
        value: String,
    },
    TailText {
        depth: usize,
        value: String,
    },
    EndElement {
        depth: usize,
        name: String,
    },
}

impl SourceRecord {
    pub const fn depth(&self) -> usize {
        match self {
            Self::StartElement { depth, .. }
            | Self::EmptyElement { depth, .. }
            | Self::ElementText { depth, .. }
            | Self::TailText { depth, .. }
            | Self::EndElement { depth, .. } => *depth,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordCounts {
    pub elements: u64,
    pub empty_elements: u64,
    pub end_elements: u64,
    pub attributes: u64,
    pub element_texts: u64,
    pub tail_texts: u64,
    pub control_characters: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSummary {
    pub schema: &'static str,
    pub sha256: String,
    pub counts: RecordCounts,
}

pub struct CanonicalDigest {
    hasher: Sha256,
    counts: RecordCounts,
}

impl Default for CanonicalDigest {
    fn default() -> Self {
        Self::new()
    }
}

impl CanonicalDigest {
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DIGEST_PREAMBLE);
        Self {
            hasher,
            counts: RecordCounts::default(),
        }
    }

    pub fn update(&mut self, record: &SourceRecord) {
        match record {
            SourceRecord::StartElement {
                depth,
                name,
                attributes,
            } => {
                self.hasher.update([0x01]);
                self.write_depth(*depth);
                self.write_string(name);
                self.write_attributes(attributes);
                self.counts.elements += 1;
                self.count_values(name, attributes);
            }
            SourceRecord::EmptyElement {
                depth,
                name,
                attributes,
            } => {
                self.hasher.update([0x02]);
                self.write_depth(*depth);
                self.write_string(name);
                self.write_attributes(attributes);
                self.counts.elements += 1;
                self.counts.empty_elements += 1;
                self.count_values(name, attributes);
            }
            SourceRecord::ElementText { depth, value } => {
                self.hasher.update([0x03]);
                self.write_depth(*depth);
                self.write_string(value);
                self.counts.element_texts += 1;
                self.count_control_characters(value);
            }
            SourceRecord::TailText { depth, value } => {
                self.hasher.update([0x04]);
                self.write_depth(*depth);
                self.write_string(value);
                self.counts.tail_texts += 1;
                self.count_control_characters(value);
            }
            SourceRecord::EndElement { depth, name } => {
                self.hasher.update([0x05]);
                self.write_depth(*depth);
                self.write_string(name);
                self.counts.end_elements += 1;
                self.count_control_characters(name);
            }
        }
    }

    pub fn finalize(self) -> DigestSummary {
        let bytes = self.hasher.finalize();
        let mut sha256 = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(sha256, "{byte:02x}").expect("writing to String cannot fail");
        }

        DigestSummary {
            schema: DIGEST_SCHEMA,
            sha256,
            counts: self.counts,
        }
    }

    fn write_attributes(&mut self, attributes: &[SourceAttribute]) {
        self.write_u64(attributes.len());
        for attribute in attributes {
            self.write_string(&attribute.name);
            self.write_string(&attribute.value);
        }
        self.counts.attributes +=
            u64::try_from(attributes.len()).expect("attribute count should fit in u64");
    }

    fn write_depth(&mut self, depth: usize) {
        self.write_u64(depth);
    }

    fn write_u64(&mut self, value: usize) {
        let value = u64::try_from(value).expect("record size should fit in u64");
        self.hasher.update(value.to_be_bytes());
    }

    fn write_string(&mut self, value: &str) {
        self.write_u64(value.len());
        self.hasher.update(value.as_bytes());
    }

    fn count_values(&mut self, name: &str, attributes: &[SourceAttribute]) {
        self.count_control_characters(name);
        for attribute in attributes {
            self.count_control_characters(&attribute.name);
            self.count_control_characters(&attribute.value);
        }
    }

    fn count_control_characters(&mut self, value: &str) {
        self.counts.control_characters += value
            .chars()
            .filter(|character| {
                let codepoint = u32::from(*character);
                codepoint < 0x20 && !matches!(codepoint, 0x09 | 0x0A | 0x0D)
            })
            .count() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::{CanonicalDigest, SourceAttribute, SourceRecord};

    fn record_with_attributes(attributes: Vec<SourceAttribute>) -> SourceRecord {
        SourceRecord::EmptyElement {
            depth: 1,
            name: "future:opaque".to_owned(),
            attributes,
        }
    }

    #[test]
    fn attribute_order_changes_digest() {
        let first = SourceAttribute {
            name: "zeta".to_owned(),
            value: "첫째".to_owned(),
        };
        let second = SourceAttribute {
            name: "alpha".to_owned(),
            value: "둘째".to_owned(),
        };

        let mut ordered = CanonicalDigest::new();
        ordered.update(&record_with_attributes(vec![first.clone(), second.clone()]));
        let mut reversed = CanonicalDigest::new();
        reversed.update(&record_with_attributes(vec![second, first]));

        assert_ne!(ordered.finalize().sha256, reversed.finalize().sha256);
    }

    #[test]
    fn record_kind_and_depth_change_digest() {
        let mut element_text = CanonicalDigest::new();
        element_text.update(&SourceRecord::ElementText {
            depth: 1,
            value: "같은 값".to_owned(),
        });
        let mut tail_text = CanonicalDigest::new();
        tail_text.update(&SourceRecord::TailText {
            depth: 1,
            value: "같은 값".to_owned(),
        });

        assert_ne!(element_text.finalize().sha256, tail_text.finalize().sha256);
    }
}
