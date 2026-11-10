//! A failed open leaves nothing behind even when it fails because the
//! process ran out of file descriptors part-way through creating a deep
//! directory tree. The descriptor limit is lowered only in a child process,
//! so it cannot starve other tests.
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use kiln_harness::audit::JsonlAudit;
use kiln_harness::journal::JsonlJournal;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

const CHILD: &str = "KILN_HARNESS_FD_EXHAUSTION_CHILD";
const TEST: &str = "running_out_of_descriptors_leaves_nothing_behind";

fn deep_output(root: &Path, depth: usize) -> PathBuf {
    (0..depth)
        .fold(root.to_path_buf(), |path, _| path.join("d"))
        .join("output.jsonl")
}

/// Run `open` with the soft descriptor limit at `limit`, then restore it.
fn with_descriptor_limit<T>(limit: u64, open: impl FnOnce() -> T) -> T {
    let original = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(limit),
            maximum: original.maximum,
        },
    )
    .expect("lower the soft descriptor limit");
    let result = open();
    setrlimit(Resource::Nofile, original).expect("restore the descriptor limit");
    result
}

fn child() {
    let temp = common::private_tempdir().expect("tempdir");
    let mut exhausted = 0;
    // Each created level keeps one descriptor for rollback and one for the
    // walk, so consecutive limits fail at different steps: the clone before
    // mkdirat, or the reopen after it.
    for limit in 24..40 {
        for sink in ["journal", "audit"] {
            let root = temp.path().join(format!("{sink}-{limit}"));
            common::dir_all_with_mode(&root, 0o700);
            let path = deep_output(&root, 80);
            let failed = with_descriptor_limit(limit, || match sink {
                "journal" => JsonlJournal::open(&path).map(drop).is_err(),
                _ => JsonlAudit::open(&path).map(drop).is_err(),
            });
            exhausted += usize::from(failed);
            let left = std::fs::read_dir(&root).expect("list root").count();
            assert!(
                !failed || left == 0,
                "{sink} at limit {limit}: a failed open left {left} entries"
            );
        }
    }
    assert!(exhausted > 0, "no open ran out of descriptors");
    println!("child: {exhausted} opens ran out of descriptors and left nothing");
}

#[test]
fn running_out_of_descriptors_leaves_nothing_behind() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("left nothing"), "{stdout}");
}
