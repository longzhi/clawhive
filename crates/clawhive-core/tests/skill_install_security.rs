use std::io::Cursor;
use std::path::Path;

use clawhive_core::{
    analyze_skill_source, install_skill_from_analysis, is_safe_relative_path, resolve_skill_source,
    unpack_tar_archive,
};

fn create_basic_skill_source(root: &Path, name: &str) -> std::path::PathBuf {
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: security test\n---\n"),
    )
    .unwrap();
    std::fs::write(source.join("README.md"), "original").unwrap();
    source
}

fn append_regular(builder: &mut tar::Builder<Vec<u8>>, path: &str, content: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new(content))
        .unwrap();
}

#[tokio::test]
async fn rejects_private_and_metadata_remote_urls() {
    let blocked_urls = [
        "http://127.0.0.1/skill.tar",
        "http://10.1.2.3/skill.tar",
        "http://192.168.1.2/skill.tar",
        "http://172.16.1.2/skill.tar",
        "http://169.254.169.254/latest/meta-data",
        "http://0.0.0.0/skill.tar",
        "http://localhost/skill.tar",
        "http://[::1]/skill.tar",
    ];

    for url in blocked_urls {
        let err = resolve_skill_source(url).await.unwrap_err().to_string();
        assert!(
            err.contains("blocked") || err.contains("private") || err.contains("localhost"),
            "url {url} should be blocked, got: {err}"
        );
    }
}

#[test]
fn tar_path_traversal_entries_are_rejected_by_safety_check() {
    assert!(!is_safe_relative_path(Path::new("../escape.txt")));
    assert!(!is_safe_relative_path(Path::new("skill/../escape.txt")));
    assert!(is_safe_relative_path(Path::new("skill/SKILL.md")));
}

#[test]
fn tar_symlink_and_hardlink_entries_are_skipped() {
    let mut builder = tar::Builder::new(Vec::new());
    append_regular(
        &mut builder,
        "skill/SKILL.md",
        b"---\nname: archive-links\ndescription: links\n---\n",
    );

    let mut symlink_header = tar::Header::new_gnu();
    symlink_header.set_entry_type(tar::EntryType::Symlink);
    symlink_header.set_size(0);
    symlink_header.set_mode(0o777);
    symlink_header.set_link_name("../../outside.txt").unwrap();
    symlink_header.set_cksum();
    builder
        .append_data(
            &mut symlink_header,
            "skill/link",
            Cursor::new(Vec::<u8>::new()),
        )
        .unwrap();

    let mut hardlink_header = tar::Header::new_gnu();
    hardlink_header.set_entry_type(tar::EntryType::Link);
    hardlink_header.set_size(0);
    hardlink_header.set_mode(0o777);
    hardlink_header.set_link_name("skill/SKILL.md").unwrap();
    hardlink_header.set_cksum();
    builder
        .append_data(
            &mut hardlink_header,
            "skill/hard",
            Cursor::new(Vec::<u8>::new()),
        )
        .unwrap();

    let tar_bytes = builder.into_inner().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    unpack_tar_archive(&mut archive, temp.path()).unwrap();

    assert!(temp.path().join("skill/SKILL.md").exists());
    assert!(!temp.path().join("skill/link").exists());
    assert!(!temp.path().join("skill/hard").exists());
}

#[test]
fn installing_same_source_twice_is_noop_for_identical_content() {
    let temp = tempfile::tempdir().unwrap();
    let source = create_basic_skill_source(temp.path(), "idempotent-skill");
    let report = analyze_skill_source(&source).unwrap();
    let config_root = temp.path().join("config");
    let skills_root = temp.path().join("skills");

    let first = install_skill_from_analysis(
        &config_root,
        &skills_root,
        &source,
        &report,
        false,
        None,
        None,
    )
    .unwrap();
    let installed_readme = first.target.join("README.md");
    std::fs::write(&installed_readme, "mutated-after-install").unwrap();

    let second = install_skill_from_analysis(
        &config_root,
        &skills_root,
        &source,
        &report,
        false,
        None,
        None,
    )
    .unwrap();

    assert_eq!(first.target, second.target);
    assert_eq!(
        std::fs::read_to_string(&installed_readme).unwrap(),
        "mutated-after-install"
    );
}
