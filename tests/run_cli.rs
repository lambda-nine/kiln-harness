//! Argument validation for `kiln-harness run`, which happens before any
//! output is opened or kilnd is contacted.
mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

const SENTINEL_KEY: &str = "sk-SENTINEL-cli-93be17";

struct Run<'a> {
    dir: &'a Path,
    args: Vec<String>,
    env: Vec<(&'a str, &'a str)>,
}

impl<'a> Run<'a> {
    fn new(dir: &'a Path) -> Self {
        Self {
            dir,
            args: [
                "--provider",
                "openai-compat",
                "--base-url",
                "http://192.168.1.20:11434/v1",
                "--model",
                "qwen3:8b",
            ]
            .map(str::to_string)
            .to_vec(),
            env: Vec::new(),
        }
    }

    fn arg(mut self, flag: &str, value: &str) -> Self {
        self.args.push(flag.to_string());
        self.args.push(value.to_string());
        self
    }

    /// Replace a default flag's value.
    fn set(mut self, flag: &str, value: &str) -> Self {
        let position = self
            .args
            .iter()
            .position(|arg| arg == flag)
            .expect("default flag");
        self.args[position + 1] = value.to_string();
        self
    }

    fn env(mut self, name: &'a str, value: &'a str) -> Self {
        self.env.push((name, value));
        self
    }

    fn output(self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kiln-harness"));
        command
            .arg("--socket")
            .arg(self.dir.join("no-kilnd.sock"))
            .arg("run")
            .args(&self.args)
            .arg("--audit")
            .arg(self.dir.join("out/audit.jsonl"))
            .arg("--journal")
            .arg(self.dir.join("out/run.jsonl"))
            .env_remove("KILN_TEST_KEY");
        for (name, value) in self.env {
            command.env(name, value);
        }
        command.output().expect("run kiln-harness")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The command failed before creating outputs, with `needle` in stderr.
fn refused(dir: &Path, output: &Output, needle: &str) {
    let text = stderr(output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains(needle), "expected {needle:?} in: {text}");
    assert!(!text.contains(SENTINEL_KEY), "{text}");
    assert!(
        !dir.join("out").exists(),
        "nothing is created before validation passes"
    );
}

#[test]
fn bad_base_urls_are_refused() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    for (url, needle) in [
        ("ftp://192.168.1.20/v1", "scheme"),
        ("http://user:secret@192.168.1.20/v1", "userinfo"),
        ("http://192.168.1.20/v1?x=1", "query"),
        ("not a url", "base URL"),
    ] {
        let output = Run::new(dir)
            .set("--base-url", url)
            .arg("--prompt", "hi")
            .output();
        refused(dir, &output, needle);
    }
    let output = Run::new(dir)
        .set("--base-url", "http://user:secret@192.168.1.20/v1")
        .arg("--prompt", "hi")
        .output();
    assert!(!stderr(&output).contains("secret"));
}

#[test]
fn exactly_one_nonempty_prompt_is_required() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let prompt_file = dir.join("prompt.txt");
    std::fs::write(&prompt_file, "from a file").expect("prompt");

    let both = Run::new(dir)
        .arg("--prompt", "hi")
        .arg("--prompt-file", prompt_file.to_str().expect("utf-8"))
        .output();
    assert_eq!(both.status.code(), Some(2), "{}", stderr(&both));
    assert!(
        stderr(&both).contains("cannot be used with"),
        "{}",
        stderr(&both)
    );

    let neither = Run::new(dir).output();
    assert_eq!(neither.status.code(), Some(2), "{}", stderr(&neither));

    for empty in ["", "  \n"] {
        let output = Run::new(dir).arg("--prompt", empty).output();
        refused(dir, &output, "the prompt is empty");
    }
    let empty_file = dir.join("empty.txt");
    std::fs::write(&empty_file, "").expect("empty");
    let output = Run::new(dir)
        .arg("--prompt-file", empty_file.to_str().expect("utf-8"))
        .output();
    refused(dir, &output, "the prompt is empty");

    let large = dir.join("large.txt");
    std::fs::write(&large, "x".repeat((64 << 10) + 1)).expect("large");
    let output = Run::new(dir)
        .arg("--prompt-file", large.to_str().expect("utf-8"))
        .output();
    refused(dir, &output, "larger than 65536 bytes");
}

#[test]
fn budgets_over_their_ceilings_are_refused() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    for (flag, value) in [
        ("--max-steps", "1001"),
        ("--max-steps", "0"),
        ("--max-tool-calls", "10001"),
        ("--max-tool-calls-per-turn", "65"),
        ("--session-timeout-secs", "21601"),
        ("--tool-timeout-secs", "601"),
        ("--max-output-tokens", "1000001"),
        ("--max-total-input-tokens", "1000000001"),
        ("--max-total-output-tokens", "1000000001"),
    ] {
        let output = Run::new(dir)
            .arg("--prompt", "hi")
            .arg(flag, value)
            .output();
        refused(dir, &output, &format!("{flag} must be between 1 and"));
    }
    for (flag, value) in [("--temperature", "2.5"), ("--model", "")] {
        let output = Run::new(dir)
            .arg("--prompt", "hi")
            .set_or_arg(flag, value)
            .output();
        refused(dir, &output, "invalid openai-compat configuration");
    }
}

impl Run<'_> {
    fn set_or_arg(self, flag: &str, value: &str) -> Self {
        if self.args.iter().any(|arg| arg == flag) {
            self.set(flag, value)
        } else {
            self.arg(flag, value)
        }
    }
}

#[test]
fn the_api_key_comes_only_from_a_named_variable() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();

    let unset = Run::new(dir)
        .arg("--prompt", "hi")
        .arg("--api-key-env", "KILN_TEST_KEY")
        .output();
    refused(dir, &unset, "KILN_TEST_KEY is not set");

    let empty = Run::new(dir)
        .arg("--prompt", "hi")
        .arg("--api-key-env", "KILN_TEST_KEY")
        .env("KILN_TEST_KEY", "")
        .output();
    refused(dir, &empty, "KILN_TEST_KEY is set but empty");

    let bad_name = Run::new(dir)
        .arg("--prompt", "hi")
        .arg("--api-key-env", "NOT=A_NAME")
        .output();
    refused(dir, &bad_name, "must name an environment variable");

    // A key over plain http to a LAN address is refused.
    let plain = Run::new(dir)
        .arg("--prompt", "hi")
        .arg("--api-key-env", "KILN_TEST_KEY")
        .env("KILN_TEST_KEY", SENTINEL_KEY)
        .output();
    refused(dir, &plain, "requires an https base URL");

    // With https the configuration is accepted, and the run stops only at
    // the missing kilnd socket, leaving no empty outputs behind.
    let https = Run::new(dir)
        .set("--base-url", "https://models.example.com/v1")
        .arg("--prompt", "hi")
        .arg("--api-key-env", "KILN_TEST_KEY")
        .env("KILN_TEST_KEY", SENTINEL_KEY)
        .output();
    let text = stderr(&https);
    assert!(!https.status.success());
    assert!(!text.contains(SENTINEL_KEY), "{text}");
    assert!(
        !text.contains("invalid openai-compat configuration"),
        "{text}"
    );
    assert!(!dir.join("out").exists(), "{text}");
}

#[test]
fn valid_arguments_reach_kilnd() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let output = Run::new(dir)
        .arg("--prompt", "List the workspace.")
        .arg("--seed", "7")
        .arg("--temperature", "0.2")
        .output();
    let text = stderr(&output);
    assert!(!output.status.success());
    // Only the absent socket stops it.
    assert!(
        !text.contains("must be") && !text.contains("invalid"),
        "{text}"
    );
    assert!(
        text.to_lowercase().contains("connect") || text.contains("no-kilnd.sock"),
        "{text}"
    );
    assert!(
        !dir.join("out").exists(),
        "outputs and their new directory are removed"
    );
}

#[test]
fn help_lists_run() {
    let output = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args([OsStr::new("run"), OsStr::new("--help")])
        .output()
        .expect("help");
    let text = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--base-url",
        "--api-key-env",
        "--prompt-file",
        "--max-steps",
    ] {
        assert!(text.contains(flag), "{text}");
    }
    assert!(text.contains("[default: 40]"), "{text}");
}
