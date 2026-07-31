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
        "fc6bcadce3eb784cab537d0203ed81101a479fe9f368648b975e1d72eee2f719"
    );
    assert_eq!(
        stdict.sha256,
        "e3190ec6c461eacec04dc21d6acb0535a98abbffbc5871bb75e90e9b20d6ae35"
    );
    assert_eq!(
        opendict.sha256,
        "eb330a77a1f1a14504536ec1b8ea72c3d246eb7196f5d42ebb9baecbf9375b35"
    );
    assert_eq!(krdict.counts.elements, 11);
    assert_eq!(krdict.counts.empty_elements, 4);
    assert_eq!(krdict.counts.attributes, 16);
    assert_eq!(stdict.counts.elements, 12);
    assert_eq!(stdict.counts.tail_texts, 1);
    assert_eq!(opendict.counts.elements, 12);
    assert_eq!(opendict.counts.empty_elements, 1);
    assert_eq!(opendict.counts.element_texts, 6);
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
