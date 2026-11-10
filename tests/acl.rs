//! How the directory rule meets access control lists.
//!
//! On Linux, POSIX ACL entries for named users and groups show in the group
//! mode bits as the ACL mask, so the mode check sees them, and setting a
//! created file's or directory's mode resets the mask, which caps what an
//! inherited default ACL grants. macOS `NFSv4` ACLs are not visible in the
//! mode bits and are not inspected; the documentation says so.
mod common;

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    use crate::common;
    use std::process::Command;

    use kiln_harness::audit::{AuditError, JsonlAudit};
    use kiln_harness::journal::{DirectoryProblem, JournalError, JsonlJournal};

    /// Apply an ACL, or say why the test cannot run here.
    fn setfacl(args: &[&str], path: &Path) -> bool {
        match Command::new("setfacl").args(args).arg(path).output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                println!(
                    "skipped: setfacl failed, so this filesystem may not support ACLs: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                false
            }
            Err(error) => {
                println!("skipped: setfacl is not installed ({error})");
                false
            }
        }
    }

    /// The permissions each named or other ACL entry of `path` effectively
    /// grants, as `(entry, permissions)`.
    fn effective_grants(path: &Path) -> Vec<(String, String)> {
        let output = Command::new("getfacl")
            .args(["--omit-header", "--absolute-names"])
            .arg(path)
            .output()
            .expect("getfacl");
        assert!(output.status.success(), "getfacl {}", path.display());
        String::from_utf8(output.stdout)
            .expect("utf8")
            .lines()
            .filter(|line| {
                !line.is_empty() && !line.starts_with("mask") && !line.starts_with("default")
            })
            .filter_map(|line| {
                let (entry, rest) = line.split_once('\t').unwrap_or((line, ""));
                let mut fields = entry.splitn(3, ':');
                let (kind, qualifier, perms) = (fields.next()?, fields.next()?, fields.next()?);
                // The owner's own entry is the point of the file.
                if kind == "user" && qualifier.is_empty() {
                    return None;
                }
                let effective = rest
                    .split("#effective:")
                    .nth(1)
                    .map_or(perms, str::trim)
                    .to_string();
                Some((format!("{kind}:{qualifier}"), effective))
            })
            .collect()
    }

    #[test]
    fn a_named_user_acl_shows_as_group_write_and_is_refused() {
        let temp = common::private_tempdir().expect("tempdir");
        let parent = temp.path().join("named-user");
        common::dir_all_with_mode(&parent, 0o700);
        if !setfacl(&["-m", "u:nobody:rwx"], &parent) {
            return;
        }

        let journal = JsonlJournal::open(&parent.join("journal.jsonl")).err();
        let audit = JsonlAudit::open(&parent.join("audit.jsonl")).err();

        assert!(
            matches!(
                journal,
                Some(JournalError::UnsafeDirectory {
                    problem: DirectoryProblem::WritableByOthers,
                    ..
                })
            ),
            "{journal:?}"
        );
        assert!(
            matches!(
                audit,
                Some(AuditError::UnsafeDirectory {
                    problem: DirectoryProblem::WritableByOthers,
                    ..
                })
            ),
            "{audit:?}"
        );
    }

    #[test]
    fn a_default_acl_grants_nobody_else_access_to_what_is_created() {
        let temp = common::private_tempdir().expect("tempdir");
        let parent = temp.path().join("default-acl");
        common::dir_all_with_mode(&parent, 0o700);
        if !setfacl(&["-d", "-m", "u:nobody:rwx"], &parent) {
            return;
        }

        let journal_path = parent.join("new/journal.jsonl");
        let audit_path = parent.join("audit.jsonl");
        let journal = JsonlJournal::open(&journal_path);
        let audit = JsonlAudit::open(&audit_path);
        if journal.is_err() || audit.is_err() {
            // Refusing is also acceptable.
            println!("refused: {:?} {:?}", journal.err(), audit.err());
            return;
        }

        for path in [&journal_path, &audit_path, &parent.join("new")] {
            for (entry, effective) in effective_grants(path) {
                assert_eq!(
                    effective,
                    "---",
                    "{} grants {entry} {effective}",
                    path.display()
                );
            }
        }
    }
}

/// Demonstrates the documented macOS limitation: an inheritable `NFSv4` ACL on
/// a mode-0700 directory is not visible in the mode bits, so the directory
/// rule accepts it, and the inherited entry survives the explicit 0600 mode.
/// Do not place journals under a directory with inheritable ACLs on macOS.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "demonstrates a documented limitation; it passes when the limitation holds"]
fn macos_inherited_acls_are_not_inspected() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use kiln_harness::audit::JsonlAudit;
    use kiln_harness::journal::JsonlJournal;

    let temp = common::private_tempdir().expect("tempdir");
    let parent = temp.path().join("acl-parent");
    common::dir_all_with_mode(&parent, 0o700);
    let status = Command::new("chmod")
        .args([
            "+a",
            "everyone allow list,search,add_file,add_subdirectory,delete_child,file_inherit,directory_inherit",
        ])
        .arg(&parent)
        .status()
        .expect("chmod +a");
    assert!(status.success());

    let journal_path = parent.join("journal.jsonl");
    let audit_path = parent.join("audit.jsonl");
    drop(JsonlJournal::open(&journal_path).expect("the mode bits pass"));
    drop(JsonlAudit::open(&audit_path).expect("the mode bits pass"));

    for path in [&journal_path, &audit_path] {
        let mode = std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        let listing = Command::new("ls")
            .arg("-le")
            .arg(path)
            .output()
            .expect("ls -le");
        let listing = String::from_utf8(listing.stdout).expect("utf8");
        assert_eq!(mode, 0o600);
        assert!(
            listing.contains("everyone inherited allow"),
            "the inherited ACL entry remains: {listing}"
        );
    }
}
