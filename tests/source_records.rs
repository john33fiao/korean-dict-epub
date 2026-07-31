use std::fs::File;
use std::path::PathBuf;

use korean_dict_epub::record::{CanonicalDigest, DigestSummary, SourceRecord};
use korean_dict_epub::source::SourceRecordReader;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("source")
        .join(name)
}

fn digest_fixture(name: &str) -> DigestSummary {
    let file = File::open(fixture(name)).expect("fixture should open");
    let mut digest = CanonicalDigest::new();
    for record in SourceRecordReader::new(file) {
        digest.update(&record.expect("fixture should parse"));
    }
    digest.finalize()
}

#[test]
fn all_dictionary_shapes_have_stable_lossless_records() {
    let krdict = digest_fixture("krdict.xml");
    let stdict = digest_fixture("stdict.xml");
    let opendict = digest_fixture("opendict.xml");

    assert_eq!(
        krdict.sha256,
        "3109b4aa9586f7f0721524a5df39fc79d6d73a1311020be75be424a07c4f7c1a"
    );
    assert_eq!(
        stdict.sha256,
        "02f8bbe17de635c919259b7b6c4ae9c3c316ee4708a2f1d3e5feed130ad694ed"
    );
    assert_eq!(
        opendict.sha256,
        "33adec182af2e8f438fefe8b247d6d86a5e2630feac5d8262b398e2e26535068"
    );
    assert_eq!(krdict.counts.elements, 6);
    assert_eq!(krdict.counts.empty_elements, 1);
    assert_eq!(krdict.counts.attributes, 8);
    assert_eq!(stdict.counts.elements, 7);
    assert_eq!(stdict.counts.tail_texts, 1);
    assert_eq!(opendict.counts.elements, 6);
    assert_eq!(opendict.counts.empty_elements, 1);
    assert_eq!(opendict.counts.element_texts, 2);
}

#[test]
fn unknown_qualified_name_and_attribute_order_are_visible() {
    let file = File::open(fixture("krdict.xml")).expect("fixture should open");
    let records = SourceRecordReader::new(file)
        .collect::<Result<Vec<_>, _>>()
        .expect("fixture should parse");

    let opaque = records
        .iter()
        .find(|record| {
            matches!(
                record,
                SourceRecord::StartElement { name, .. } if name == "future:opaque"
            )
        })
        .expect("unknown qualified element should be retained");
    let SourceRecord::StartElement { attributes, .. } = opaque else {
        unreachable!("matched a start element")
    };

    assert_eq!(attributes[0].name, "zeta");
    assert_eq!(attributes[1].name, "alpha");
}
