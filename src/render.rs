use crate::record::{SourceAttribute, SourceRecord};

pub const BOOK_CSS: &str = r#"
:root {
  color-scheme: light dark;
}

html,
body {
  margin: 0;
  padding: 0;
}

body {
  font-family: "Noto Serif CJK KR", "Noto Serif KR", serif;
  line-height: 1.7;
  padding: 1.25rem;
  text-align: start;
  word-break: keep-all;
  overflow-wrap: anywhere;
}

h1,
h2 {
  line-height: 1.35;
  text-align: start;
}

.book-summary {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 0.3rem 0.8rem;
}

.book-summary dt {
  font-weight: 700;
}

.book-summary dd {
  margin: 0;
}

.entry {
  border-block-start: 1px solid #999;
  margin-block: 1.5rem;
  padding-block-start: 1rem;
}

.entry-heading {
  font-size: 1.25rem;
  margin-block: 0 0.7rem;
}

.xml-record {
  margin-inline-start: min(calc(var(--xml-depth, 0) * 0.6rem), 4.8rem);
  padding-block: 0.12rem;
}

.xml-token,
.xml-name,
.xml-attribute-name {
  font-family: ui-monospace, monospace;
}

.xml-attribute {
  margin-inline-start: 0.55rem;
}

.xml-text-value,
.xml-tail-value {
  white-space: pre-wrap;
}

.xml-tail {
  color: #666;
  font-style: italic;
}

.xml-end {
  color: #777;
  font-size: 0.9em;
}

.semantic-headword {
  font-weight: 700;
}

.semantic-definition {
  margin-block-start: 0.3rem;
}

.semantic-example {
  color: #555;
}

.semantic-translation {
  border-inline-start: 0.2rem solid #999;
  padding-inline-start: 0.5rem;
}

.xml-control {
  border: 1px solid #b44;
  border-radius: 0.2rem;
  color: #b22;
  font-family: ui-monospace, monospace;
  margin-inline: 0.15rem;
  padding-inline: 0.2rem;
}

nav ol {
  padding-inline-start: 1.5rem;
}

nav li {
  margin-block: 0.35rem;
}
"#;

pub fn render_record(record: &SourceRecord) -> String {
    match record {
        SourceRecord::StartElement {
            depth,
            name,
            attributes,
        } => render_element("start", "xml-start", *depth, name, attributes, false),
        SourceRecord::EmptyElement {
            depth,
            name,
            attributes,
        } => render_element("empty", "xml-empty", *depth, name, attributes, true),
        SourceRecord::ElementText { depth, value } => format!(
            "<div class=\"xml-record xml-text\" data-kdep-record=\"true\" \
             data-kdep-kind=\"text\" data-kdep-depth=\"{depth}\" \
             style=\"--xml-depth: {depth}\"><span class=\"xml-text-value\">{}</span></div>\n",
            render_value(value)
        ),
        SourceRecord::TailText { depth, value } => format!(
            "<div class=\"xml-record xml-tail\" data-kdep-record=\"true\" \
             data-kdep-kind=\"tail\" data-kdep-depth=\"{depth}\" \
             style=\"--xml-depth: {depth}\"><span class=\"xml-tail-value\">{}</span></div>\n",
            render_value(value)
        ),
        SourceRecord::EndElement { depth, name } => format!(
            "<div class=\"xml-record xml-end\" data-kdep-record=\"true\" \
             data-kdep-kind=\"end\" data-kdep-depth=\"{depth}\" \
             style=\"--xml-depth: {depth}\"><span class=\"xml-token\">&lt;/</span>\
             <code class=\"xml-name\">{}</code><span class=\"xml-token\">&gt;</span></div>\n",
            escape_xml(name)
        ),
    }
}

pub fn render_value(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    let mut plain = String::new();

    for character in value.chars() {
        let codepoint = u32::from(character);
        if codepoint < 0x20 && !matches!(codepoint, 0x09 | 0x0A | 0x0D) {
            rendered.push_str(&escape_xml(&plain));
            plain.clear();
            let label = format!("U+{codepoint:04X}");
            let symbol = char::from_u32(0x2400 + codepoint).unwrap_or('�');
            rendered.push_str(&format!(
                "<span class=\"xml-control\" data-codepoint=\"{label}\">{symbol} {label}</span>"
            ));
        } else {
            plain.push(character);
        }
    }
    rendered.push_str(&escape_xml(&plain));
    rendered
}

pub fn xhtml_document(title: &str, body: &str, stylesheet_href: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" \
         xmlns:epub=\"http://www.idpf.org/2007/ops\" lang=\"ko\" xml:lang=\"ko\">\n\
         <head>\n<meta charset=\"UTF-8\" />\n<title>{}</title>\n\
         <link rel=\"stylesheet\" type=\"text/css\" href=\"{}\" />\n\
         </head>\n<body>\n{body}\n</body>\n</html>\n",
        escape_xml(title),
        escape_xml(stylesheet_href)
    )
}

pub fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn render_element(
    kind: &str,
    base_class: &str,
    depth: usize,
    name: &str,
    attributes: &[SourceAttribute],
    empty: bool,
) -> String {
    let semantic = semantic_class(name, attributes);
    let semantic = semantic.map_or(String::new(), |class| format!(" {class}"));
    let attributes = attributes.iter().map(render_attribute).collect::<String>();
    let closing = if empty { "/&gt;" } else { "&gt;" };

    format!(
        "<div class=\"xml-record {base_class}{semantic}\" data-kdep-record=\"true\" \
         data-kdep-kind=\"{kind}\" data-kdep-depth=\"{depth}\" \
         style=\"--xml-depth: {depth}\"><span class=\"xml-token\">&lt;</span>\
         <code class=\"xml-name\">{}</code>{attributes}\
         <span class=\"xml-token\">{closing}</span></div>\n",
        escape_xml(name)
    )
}

fn render_attribute(attribute: &SourceAttribute) -> String {
    format!(
        "<span class=\"xml-attribute\"><code class=\"xml-attribute-name\">{}</code>\
         <span class=\"xml-token\">=&quot;</span><span class=\"xml-attribute-value\">{}</span>\
         <span class=\"xml-token\">&quot;</span></span>",
        escape_xml(&attribute.name),
        render_value(&attribute.value)
    )
}

fn semantic_class(name: &str, attributes: &[SourceAttribute]) -> Option<&'static str> {
    let local = local_name(name);
    if local == "feat"
        && attributes
            .iter()
            .any(|attribute| attribute.name == "att" && attribute.value == "writtenForm")
    {
        return Some("semantic-headword");
    }

    match local.to_ascii_lowercase().as_str() {
        "word" | "headword" | "lemma" => Some("semantic-headword"),
        "definition" | "definition_original" | "sense" => Some("semantic-definition"),
        "example" | "example_info" | "exampleinfo" => Some("semantic-example"),
        "translation" | "translation_info" | "translationinfo" => Some("semantic-translation"),
        _ => None,
    }
}

pub fn local_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit_once(':')
        .map_or(qualified_name, |(_, local)| local)
}

#[cfg(test)]
mod tests {
    use crate::record::{SourceAttribute, SourceRecord};

    use super::{BOOK_CSS, render_record, render_value};

    #[test]
    fn controls_are_visible_and_reversible_in_xhtml() {
        let rendered = render_value("앞\u{0008}뒤");

        assert!(rendered.contains("class=\"xml-control\""));
        assert!(rendered.contains("data-codepoint=\"U+0008\""));
        assert!(rendered.contains("앞"));
        assert!(rendered.contains("뒤"));
        assert!(!rendered.contains('\u{0008}'));
    }

    #[test]
    fn unknown_elements_and_original_attribute_order_are_rendered() {
        let record = SourceRecord::EmptyElement {
            depth: 2,
            name: "future:opaque".to_owned(),
            attributes: vec![
                SourceAttribute {
                    name: "zeta".to_owned(),
                    value: "첫째".to_owned(),
                },
                SourceAttribute {
                    name: "alpha".to_owned(),
                    value: "둘째".to_owned(),
                },
            ],
        };

        let rendered = render_record(&record);

        assert!(rendered.contains("future:opaque"));
        assert!(
            rendered.find("zeta").expect("zeta should render")
                < rendered.find("alpha").expect("alpha should render")
        );
        assert!(rendered.contains("data-kdep-kind=\"empty\""));
    }

    #[test]
    fn css_contains_required_small_screen_rules() {
        assert!(BOOK_CSS.contains("word-break: keep-all"));
        assert!(BOOK_CSS.contains("overflow-wrap: anywhere"));
        assert!(BOOK_CSS.contains("text-align: start"));
    }
}
