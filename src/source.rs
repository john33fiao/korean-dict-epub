use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io::{self, BufReader, Read};
use std::str;

use quick_xml::XmlVersion;
use quick_xml::encoding::EncodingError;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::reader::Reader;

use crate::record::{SourceAttribute, SourceRecord};

const XML_BUFFER_CAPACITY: usize = 64 * 1024;
const SANITIZER_INPUT_CAPACITY: usize = 8 * 1024;
const CONTROL_ESCAPE: char = '\u{E000}';
const CONTROL_REPLACEMENT_BASE: u32 = 0xE010;
const FORBIDDEN_XML_CONTROLS: [u8; 29] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0B, 0x0C, 0x0E, 0x0F, 0x10, 0x11, 0x12,
    0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
];

#[derive(Debug)]
pub enum SourceError {
    Io(io::Error),
    Xml(quick_xml::Error),
    Encoding(EncodingError),
    InvalidUtf8(str::Utf8Error),
    InvalidControlEscape,
    TextOutsideRoot(String),
    UnrecognizedEntity(String),
    UnexpectedEof,
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error while reading XML: {error}"),
            Self::Xml(error) => write!(formatter, "XML parse error: {error}"),
            Self::Encoding(error) => write!(formatter, "XML decoding error: {error}"),
            Self::InvalidUtf8(error) => write!(formatter, "XML is not valid UTF-8: {error}"),
            Self::InvalidControlEscape => {
                formatter.write_str("XML control-character escape sequence is invalid")
            }
            Self::TextOutsideRoot(value) => {
                write!(
                    formatter,
                    "meaningful text appears outside the root element: {value:?}"
                )
            }
            Self::UnrecognizedEntity(name) => {
                write!(formatter, "unrecognized XML entity reference: &{name};")
            }
            Self::UnexpectedEof => formatter.write_str("XML ended with unclosed elements"),
        }
    }
}

impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Xml(error) => Some(error),
            Self::Encoding(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::InvalidControlEscape
            | Self::TextOutsideRoot(_)
            | Self::UnrecognizedEntity(_)
            | Self::UnexpectedEof => None,
        }
    }
}

impl From<io::Error> for SourceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<quick_xml::Error> for SourceError {
    fn from(error: quick_xml::Error) -> Self {
        Self::Xml(error)
    }
}

impl From<EncodingError> for SourceError {
    fn from(error: EncodingError) -> Self {
        Self::Encoding(error)
    }
}

impl From<str::Utf8Error> for SourceError {
    fn from(error: str::Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}

#[derive(Debug)]
struct Frame {
    depth: usize,
    child_count: usize,
    text: String,
}

pub struct SourceRecordReader<R: Read> {
    reader: Reader<BufReader<ControlSanitizer<R>>>,
    buffer: Vec<u8>,
    frames: Vec<Frame>,
    pending: VecDeque<SourceRecord>,
    finished: bool,
}

impl<R: Read> SourceRecordReader<R> {
    pub fn new(input: R) -> Self {
        let sanitized = ControlSanitizer::new(input);
        let buffered = BufReader::with_capacity(XML_BUFFER_CAPACITY, sanitized);
        let mut reader = Reader::from_reader(buffered);
        reader.config_mut().enable_all_checks(true);

        Self {
            reader,
            buffer: Vec::with_capacity(XML_BUFFER_CAPACITY),
            frames: Vec::new(),
            pending: VecDeque::new(),
            finished: false,
        }
    }

    fn read_next_event(&mut self) -> Result<(), SourceError> {
        self.buffer.clear();
        match self.reader.read_event_into(&mut self.buffer)? {
            Event::Start(element) => {
                let element = element.into_owned();
                self.before_child()?;
                let depth = self.frames.len();
                let name = decode_name(element.name().as_ref())?;
                let attributes = decode_attributes(&element)?;
                self.frames.push(Frame {
                    depth,
                    child_count: 0,
                    text: String::new(),
                });
                self.pending.push_back(SourceRecord::StartElement {
                    depth,
                    name,
                    attributes,
                });
            }
            Event::Empty(element) => {
                let element = element.into_owned();
                self.before_child()?;
                let depth = self.frames.len();
                self.pending.push_back(SourceRecord::EmptyElement {
                    depth,
                    name: decode_name(element.name().as_ref())?,
                    attributes: decode_attributes(&element)?,
                });
            }
            Event::Text(text) => {
                let value = restore_control_characters(text.xml10_content()?.as_ref())?;
                self.append_text(&value)?;
            }
            Event::CData(text) => {
                let value = restore_control_characters(text.xml10_content()?.as_ref())?;
                self.append_text(&value)?;
            }
            Event::GeneralRef(reference) => {
                let value = resolve_reference(&reference)?;
                self.append_text(&value)?;
            }
            Event::End(element) => {
                let element = element.into_owned();
                self.flush_current_text()?;
                let frame = self.frames.pop().ok_or(SourceError::UnexpectedEof)?;
                let name = decode_name(element.name().as_ref())?;
                self.pending.push_back(SourceRecord::EndElement {
                    depth: frame.depth,
                    name,
                });
            }
            Event::Eof => {
                if !self.frames.is_empty() {
                    return Err(SourceError::UnexpectedEof);
                }
                self.finished = true;
            }
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_) => {}
        }
        Ok(())
    }

    fn before_child(&mut self) -> Result<(), SourceError> {
        if self.frames.is_empty() {
            return Ok(());
        }
        self.flush_current_text()?;
        self.frames.last_mut().expect("a frame exists").child_count += 1;
        Ok(())
    }

    fn append_text(&mut self, value: &str) -> Result<(), SourceError> {
        if let Some(frame) = self.frames.last_mut() {
            frame.text.push_str(value);
            return Ok(());
        }
        if value.chars().all(char::is_whitespace) {
            Ok(())
        } else {
            Err(SourceError::TextOutsideRoot(value.to_owned()))
        }
    }

    fn flush_current_text(&mut self) -> Result<(), SourceError> {
        let Some(frame) = self.frames.last_mut() else {
            return Ok(());
        };
        if frame.text.is_empty() || frame.text.chars().all(char::is_whitespace) {
            frame.text.clear();
            return Ok(());
        }

        let value = std::mem::take(&mut frame.text);
        let record = if frame.child_count == 0 {
            SourceRecord::ElementText {
                depth: frame.depth,
                value,
            }
        } else {
            SourceRecord::TailText {
                depth: frame.depth + 1,
                value,
            }
        };
        self.pending.push_back(record);
        Ok(())
    }
}

impl<R: Read> Iterator for SourceRecordReader<R> {
    type Item = Result<SourceRecord, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.pending.pop_front() {
                return Some(Ok(record));
            }
            if self.finished {
                return None;
            }
            if let Err(error) = self.read_next_event() {
                self.finished = true;
                return Some(Err(error));
            }
        }
    }
}

fn decode_name(bytes: &[u8]) -> Result<String, SourceError> {
    restore_control_characters(str::from_utf8(bytes)?)
}

fn decode_attributes(element: &BytesStart<'_>) -> Result<Vec<SourceAttribute>, SourceError> {
    let mut attributes = Vec::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        attributes.push(SourceAttribute {
            name: decode_name(attribute.key.as_ref())?,
            value: restore_control_characters(
                attribute
                    .normalized_value(XmlVersion::Implicit1_0)?
                    .as_ref(),
            )?,
        });
    }
    Ok(attributes)
}

fn resolve_reference(reference: &BytesRef<'_>) -> Result<String, SourceError> {
    if let Some(character) = reference.resolve_char_ref()? {
        return restore_control_characters(&character.to_string());
    }

    let name = reference.xml10_content()?;
    let value = match name.as_ref() {
        "lt" => "<",
        "gt" => ">",
        "amp" => "&",
        "apos" => "'",
        "quot" => "\"",
        other => return Err(SourceError::UnrecognizedEntity(other.to_owned())),
    };
    Ok(value.to_owned())
}

fn restore_control_characters(value: &str) -> Result<String, SourceError> {
    if !value.contains(CONTROL_ESCAPE) {
        return Ok(value.to_owned());
    }

    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != CONTROL_ESCAPE {
            output.push(character);
            continue;
        }

        let escaped = characters.next().ok_or(SourceError::InvalidControlEscape)?;
        if escaped == CONTROL_ESCAPE {
            output.push(CONTROL_ESCAPE);
            continue;
        }
        let codepoint = u32::from(escaped);
        let Some(original) = codepoint
            .checked_sub(CONTROL_REPLACEMENT_BASE)
            .filter(|value| *value < 0x20)
        else {
            return Err(SourceError::InvalidControlEscape);
        };
        let original = char::from_u32(original).ok_or(SourceError::InvalidControlEscape)?;
        output.push(original);
    }
    Ok(output)
}

struct ControlSanitizer<R: Read> {
    input: R,
    input_buffer: [u8; SANITIZER_INPUT_CAPACITY],
    output: Vec<u8>,
    output_position: usize,
    escape_match: Vec<u8>,
    eof: bool,
}

impl<R: Read> ControlSanitizer<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            input_buffer: [0; SANITIZER_INPUT_CAPACITY],
            output: Vec::with_capacity(SANITIZER_INPUT_CAPACITY * 2),
            output_position: 0,
            escape_match: Vec::with_capacity(CONTROL_ESCAPE.len_utf8()),
            eof: false,
        }
    }

    fn fill_output(&mut self) -> io::Result<()> {
        self.output.clear();
        self.output_position = 0;
        let count = self.input.read(&mut self.input_buffer)?;
        if count == 0 {
            self.output.append(&mut self.escape_match);
            self.eof = true;
            return Ok(());
        }

        let escape_bytes = CONTROL_ESCAPE.to_string().into_bytes();
        for index in 0..count {
            let byte = self.input_buffer[index];
            if byte == escape_bytes[self.escape_match.len()] {
                self.escape_match.push(byte);
                if self.escape_match.len() == escape_bytes.len() {
                    self.output.extend_from_slice(&escape_bytes);
                    self.output.extend_from_slice(&escape_bytes);
                    self.escape_match.clear();
                }
                continue;
            }

            self.output.append(&mut self.escape_match);
            if byte == escape_bytes[0] {
                self.escape_match.push(byte);
            } else if FORBIDDEN_XML_CONTROLS.contains(&byte) {
                self.output.extend_from_slice(&escape_bytes);
                let replacement = char::from_u32(CONTROL_REPLACEMENT_BASE + u32::from(byte))
                    .expect("replacement is a valid Unicode scalar");
                let mut encoded = [0; 4];
                self.output
                    .extend_from_slice(replacement.encode_utf8(&mut encoded).as_bytes());
            } else {
                self.output.push(byte);
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for ControlSanitizer<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.output_position == self.output.len() && !self.eof {
            self.fill_output()?;
        }
        if self.output_position == self.output.len() {
            return Ok(0);
        }

        let available = &self.output[self.output_position..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.output_position += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use crate::record::{CanonicalDigest, SourceAttribute, SourceRecord};

    use super::{CONTROL_ESCAPE, SourceRecordReader};

    fn records(bytes: &[u8]) -> Vec<SourceRecord> {
        SourceRecordReader::new(Cursor::new(bytes))
            .collect::<Result<Vec<_>, _>>()
            .expect("fixture should parse")
    }

    #[test]
    fn preserves_attribute_order_empty_elements_text_and_tail() {
        let xml = br#"<root zeta="first" alpha="second">before<unknown flag="yes"/>after</root>"#;

        assert_eq!(
            records(xml),
            vec![
                SourceRecord::StartElement {
                    depth: 0,
                    name: "root".to_owned(),
                    attributes: vec![
                        SourceAttribute {
                            name: "zeta".to_owned(),
                            value: "first".to_owned(),
                        },
                        SourceAttribute {
                            name: "alpha".to_owned(),
                            value: "second".to_owned(),
                        },
                    ],
                },
                SourceRecord::ElementText {
                    depth: 0,
                    value: "before".to_owned(),
                },
                SourceRecord::EmptyElement {
                    depth: 1,
                    name: "unknown".to_owned(),
                    attributes: vec![SourceAttribute {
                        name: "flag".to_owned(),
                        value: "yes".to_owned(),
                    }],
                },
                SourceRecord::TailText {
                    depth: 1,
                    value: "after".to_owned(),
                },
                SourceRecord::EndElement {
                    depth: 0,
                    name: "root".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn joins_text_split_by_cdata_comments_and_references() {
        let xml = br#"<root>one<![CDATA[ two]]><!-- ignored --> &amp; three</root>"#;
        let parsed = records(xml);

        assert_eq!(
            parsed[1],
            SourceRecord::ElementText {
                depth: 0,
                value: "one two & three".to_owned(),
            }
        );
    }

    #[test]
    fn raw_control_and_real_escape_marker_round_trip_without_collision() {
        let marker = CONTROL_ESCAPE.to_string();
        let mut xml = format!("<root>{marker}before").into_bytes();
        xml.push(0x08);
        xml.extend_from_slice(format!("after{marker}</root>").as_bytes());

        let parsed = records(&xml);

        assert_eq!(
            parsed[1],
            SourceRecord::ElementText {
                depth: 0,
                value: format!("{marker}before\u{0008}after{marker}"),
            }
        );
        let mut digest = CanonicalDigest::new();
        for record in &parsed {
            digest.update(record);
        }
        assert_eq!(digest.finalize().counts.control_characters, 1);
    }

    #[test]
    fn raw_control_in_attribute_is_restored_in_original_position() {
        let mut xml = Vec::from("<root value=\"앞".as_bytes());
        xml.push(0x08);
        xml.extend_from_slice("\" other=\"뒤\"/>".as_bytes());

        let parsed = records(&xml);
        let SourceRecord::EmptyElement { attributes, .. } = &parsed[0] else {
            panic!("fixture root should be an empty element")
        };

        assert_eq!(attributes[0].name, "value");
        assert_eq!(attributes[0].value, "앞\u{0008}");
        assert_eq!(attributes[1].name, "other");
        assert_eq!(attributes[1].value, "뒤");
    }

    #[test]
    fn marker_split_across_reads_is_still_escaped() {
        struct OneByteReader(Cursor<Vec<u8>>);

        impl Read for OneByteReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let mut single = [0];
                let count = self.0.read(&mut single)?;
                if count == 1 && !buffer.is_empty() {
                    buffer[0] = single[0];
                    Ok(1)
                } else {
                    Ok(0)
                }
            }
        }

        let marker = CONTROL_ESCAPE.to_string();
        let xml = format!("<root>{marker}</root>").into_bytes();
        let parsed = SourceRecordReader::new(OneByteReader(Cursor::new(xml)))
            .collect::<Result<Vec<_>, _>>()
            .expect("chunked fixture should parse");

        assert_eq!(
            parsed[1],
            SourceRecord::ElementText {
                depth: 0,
                value: marker,
            }
        );
    }

    #[test]
    fn large_stream_is_consumed_in_bounded_input_chunks() {
        struct GuardedReader(Cursor<Vec<u8>>);

        impl Read for GuardedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(
                    buffer.len() <= super::SANITIZER_INPUT_CAPACITY,
                    "source reader requested an unbounded input buffer"
                );
                self.0.read(buffer)
            }
        }

        let mut xml = Vec::from("<root>".as_bytes());
        for _ in 0..100_000 {
            xml.extend_from_slice(b"<item/>");
        }
        xml.extend_from_slice(b"</root>");

        let mut count = 0;
        for record in SourceRecordReader::new(GuardedReader(Cursor::new(xml))) {
            record.expect("large fixture should stream");
            count += 1;
        }

        assert_eq!(count, 100_002);
    }
}
