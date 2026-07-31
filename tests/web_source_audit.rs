use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use korean_dict_epub::web_source_audit::{
    EntityKind, RelationStatus, WebSourceAuditError, run_audit,
};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "korean-dict-web-source-audit-{}-{sequence}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("stale fixture should be removable");
        }
        for dictionary in ["krdict", "stdict", "opendict"] {
            fs::create_dir_all(root.join(dictionary)).expect("dictionary directory should exist");
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/web_source_audit")
                .join(format!("{dictionary}.xml"));
            fs::copy(source, root.join(dictionary).join("001.xml"))
                .expect("fixture XML should copy");
        }
        fs::write(
            root.join("opendict/untracked.xml"),
            "<channel><item><target_code>999999</target_code></item></channel>",
        )
        .expect("untracked XML should be written");
        run_git(&root, &["init", "--quiet"]);
        run_git(
            &root,
            &[
                "add",
                "krdict/001.xml",
                "stdict/001.xml",
                "opendict/001.xml",
            ],
        );
        run_git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_git(root: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .status()
        .expect("Git should start");
    assert!(
        status.success(),
        "Git command should succeed: {arguments:?}"
    );
}

#[test]
fn audits_three_dictionary_namespaces_relations_and_cycles_deterministically() {
    let fixture = Fixture::new();
    let output = fixture.path().parent().unwrap().join(format!(
        "korean-dict-web-source-audit-output-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let report = run_audit(fixture.path(), &output, false).expect("audit should pass");

    assert_eq!(
        report.input_files.len(),
        3,
        "untracked XML must be excluded"
    );
    assert_eq!(report.dictionary_entry_counts["krdict"], 4);
    assert_eq!(report.dictionary_entry_counts["stdict"], 2);
    assert_eq!(report.dictionary_entry_counts["opendict"], 2);

    let krdict = &report.relation_status_counts["krdict"];
    assert_eq!(
        (
            krdict.total,
            krdict.resolved,
            krdict.self_reference,
            krdict.unresolved,
            krdict.ambiguous
        ),
        (5, 2, 1, 1, 1)
    );
    let stdict = &report.relation_status_counts["stdict"];
    assert_eq!((stdict.total, stdict.resolved, stdict.ambiguous), (5, 4, 1));
    let opendict = &report.relation_status_counts["opendict"];
    assert_eq!(
        (
            opendict.total,
            opendict.resolved,
            opendict.self_reference,
            opendict.unresolved
        ),
        (4, 2, 1, 1)
    );
    assert_eq!(report.cycle_summary.groups, 2);
    assert_eq!(report.cycle_summary.relation_edges, 4);

    let krdict_entries = report
        .namespaces
        .iter()
        .find(|namespace| {
            namespace.dictionary == "krdict" && namespace.entity_kind == EntityKind::Entry
        })
        .expect("krdict entry namespace should exist");
    assert_eq!(krdict_entries.global_duplicate_values, 1);
    let opendict_senses = report
        .namespaces
        .iter()
        .find(|namespace| {
            namespace.dictionary == "opendict" && namespace.entity_kind == EntityKind::Sense
        })
        .expect("opendict sense namespace should exist");
    assert_eq!(opendict_senses.global_duplicate_values, 1);
    assert_eq!(opendict_senses.within_entry.duplicate_values, 0);
    assert!(report.relations.iter().any(|relation| {
        relation.status == RelationStatus::Ambiguous
            && relation.reason.contains("word_no conflicts")
    }));

    let json_path = output.join("source-identifiers-relations-v1.json");
    let markdown_path = output.join("source-identifiers-relations-v1.md");
    let first_json = fs::read(&json_path).expect("JSON report should exist");
    let first_markdown = fs::read(&markdown_path).expect("Markdown report should exist");
    let error = run_audit(fixture.path(), &output, false).expect_err("overwrite must be explicit");
    assert!(matches!(error, WebSourceAuditError::ExistingOutput(_)));
    run_audit(fixture.path(), &output, true).expect("explicit overwrite should pass");
    assert_eq!(first_json, fs::read(&json_path).unwrap());
    assert_eq!(first_markdown, fs::read(&markdown_path).unwrap());

    let unsafe_error = run_audit(fixture.path(), &fixture.path().join("generated"), false)
        .expect_err("output inside source must be rejected");
    assert!(matches!(
        unsafe_error,
        WebSourceAuditError::UnsafeOutput { .. }
    ));
    fs::remove_dir_all(&output).expect("test output should be removable");
}

#[test]
fn malformed_tracked_xml_fails_without_creating_reports() {
    let fixture = Fixture::new();
    fs::write(fixture.path().join("opendict/001.xml"), "<channel><item>")
        .expect("malformed XML should be written");
    run_git(fixture.path(), &["add", "opendict/001.xml"]);
    run_git(
        fixture.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "malformed",
        ],
    );
    let output = fixture.path().parent().unwrap().join(format!(
        "korean-dict-web-source-audit-malformed-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let error = run_audit(fixture.path(), &output, false).expect_err("malformed XML must fail");
    assert!(matches!(error, WebSourceAuditError::Source { .. }));
    assert!(!output.exists());
}

#[test]
fn modified_tracked_xml_is_rejected_before_it_can_mislabel_the_corpus() {
    let fixture = Fixture::new();
    fs::write(fixture.path().join("krdict/001.xml"), "<changed/>")
        .expect("tracked XML should be modified");
    let output = fixture.path().parent().unwrap().join(format!(
        "korean-dict-web-source-audit-dirty-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let error = run_audit(fixture.path(), &output, false)
        .expect_err("modified tracked XML must be rejected");
    assert!(matches!(error, WebSourceAuditError::DirtySource(_)));
    assert!(!output.exists());
}

#[test]
fn standalone_binary_help_is_available() {
    let output = Command::new(env!("CARGO_BIN_EXE_korean-dict-web-source-audit"))
        .arg("--help")
        .output()
        .expect("audit binary should start");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("without creating a database"));
    assert!(stdout.contains("--overwrite"));
}
