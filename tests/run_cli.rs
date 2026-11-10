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
        ("--max-total-output-bytes", "268435457"),
        ("--max-total-output-bytes", "0"),
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
    assert!(text.contains("[default: 16384]"), "{text}");
    assert!(text.contains("[default: 600000]"), "{text}");
    assert!(text.contains("--max-total-output-bytes"), "{text}");
    assert!(text.contains("[default: 8388608]"), "{text}");
}

#[test]
fn prompt_and_system_files_must_be_regular_files() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let fifo = dir.join("prompt.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(made.success());
    for flag in ["--prompt-file", "--system-file"] {
        let started = std::time::Instant::now();
        let mut run = Run::new(dir);
        if flag == "--system-file" {
            run = run.arg("--prompt", "hi");
        }
        // A FIFO with no writer would block a plain open forever.
        let output = run.arg(flag, fifo.to_str().expect("utf-8")).output();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        refused(dir, &output, "is not a regular file");
    }

    let target = dir.join("real.txt");
    std::fs::write(&target, "a task").expect("target");
    let link = dir.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let output = Run::new(dir)
        .arg("--prompt-file", link.to_str().expect("utf-8"))
        .output();
    refused(
        dir,
        &output,
        "symlink as the last path component is refused",
    );
    let output = Run::new(dir)
        .arg("--prompt", "hi")
        .arg("--system-file", dir.to_str().expect("utf-8"))
        .output();
    refused(dir, &output, "is not a regular file");
}

#[test]
fn an_oversized_prompt_file_is_refused_without_reading_it() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    // Sparse: 8 GiB by size, nothing on disk. Reading it whole would take
    // far longer than the bound below.
    let huge = dir.join("huge.txt");
    std::fs::File::create(&huge)
        .and_then(|file| file.set_len(8 << 30))
        .expect("sparse file");
    let started = std::time::Instant::now();
    let output = Run::new(dir)
        .arg("--prompt-file", huge.to_str().expect("utf-8"))
        .output();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    refused(dir, &output, "is larger than 65536 bytes");
}

impl<'a> Run<'a> {
    /// `run --provider anthropic` with only its required key variable.
    fn anthropic(dir: &'a Path) -> Self {
        Self {
            dir,
            args: ["--provider", "anthropic", "--api-key-env", "KILN_TEST_KEY"]
                .map(str::to_string)
                .to_vec(),
            env: Vec::new(),
        }
    }

    fn flag(mut self, flag: &str) -> Self {
        self.args.push(flag.to_string());
        self
    }
}

#[test]
fn provider_specific_flags_are_refused_for_the_other_provider() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    for (flag, value) in [
        ("--effort", Some("high")),
        ("--strict-tools", None),
        ("--no-fallbacks", None),
    ] {
        let run = Run::new(dir).arg("--prompt", "hi");
        let run = match value {
            Some(value) => run.arg(flag, value),
            None => run.flag(flag),
        };
        refused(
            dir,
            &run.output(),
            &format!("{flag} does not apply to --provider openai-compat"),
        );
    }
    for (flag, value) in [("--temperature", "0.5"), ("--seed", "7")] {
        let output = Run::anthropic(dir)
            .env("KILN_TEST_KEY", SENTINEL_KEY)
            .arg("--prompt", "hi")
            .arg(flag, value)
            .output();
        refused(
            dir,
            &output,
            &format!("{flag} does not apply to --provider anthropic"),
        );
    }
}

#[test]
fn anthropic_needs_a_key_or_a_proxy_and_a_valid_effort() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let mut without_key = Run::anthropic(dir).arg("--prompt", "hi");
    without_key.args.drain(2..4);
    refused(
        dir,
        &without_key.output(),
        "--provider anthropic needs --api-key-env, or --base-url pointing at a \
         credential-injecting proxy",
    );
    let unset = Run::anthropic(dir).arg("--prompt", "hi").output();
    refused(dir, &unset, "KILN_TEST_KEY is not set");

    let bad_effort = Run::anthropic(dir)
        .env("KILN_TEST_KEY", SENTINEL_KEY)
        .arg("--prompt", "hi")
        .arg("--effort", "extreme")
        .output();
    assert_eq!(bad_effort.status.code(), Some(2), "{}", stderr(&bad_effort));
    assert!(stderr(&bad_effort).contains("effort must be low, medium, high, xhigh, or max"));

    // A key over plain http to a remote host is refused like openai-compat.
    let cleartext = Run::anthropic(dir)
        .env("KILN_TEST_KEY", SENTINEL_KEY)
        .arg("--prompt", "hi")
        .arg("--base-url", "http://api.example.com")
        .output();
    refused(dir, &cleartext, "requires an https base URL");
}

#[test]
fn openai_compat_still_needs_a_base_url_and_model() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    for flag in ["--base-url", "--model"] {
        let mut run = Run::new(dir).arg("--prompt", "hi");
        let position = run.args.iter().position(|arg| arg == flag).expect("flag");
        run.args.drain(position..position + 2);
        refused(
            dir,
            &run.output(),
            "--provider openai-compat needs --base-url and --model",
        );
    }
}

#[test]
fn keyless_anthropic_needs_a_proxy_and_may_use_plain_http() {
    let temp = common::private_tempdir().expect("tempdir");
    let dir = temp.path();
    let keyless = |base_url: Option<&str>| {
        let mut run = Run::anthropic(dir).arg("--prompt", "hi");
        run.args.drain(2..4);
        match base_url {
            Some(url) => run.arg("--base-url", url),
            None => run,
        }
    };
    let needle = "--provider anthropic needs --api-key-env, or --base-url pointing at a \
                  credential-injecting proxy";
    for direct in [
        None,
        Some("https://api.anthropic.com"),
        Some("https://API.anthropic.com./"),
    ] {
        refused(dir, &keyless(direct).output(), needle);
    }
    // A keyless proxy over plain http, even off loopback, passes every check
    // and fails only at the missing kilnd socket, after the outputs open.
    for proxy in ["http://host.lima.internal:8788", "http://127.0.0.1:8788"] {
        let output = keyless(Some(proxy)).output();
        let text = stderr(&output);
        assert!(!output.status.success());
        assert!(!text.contains(needle), "{text}");
        assert!(text.contains("kilnd"), "{text}");
        assert!(!text.contains("https"), "{text}");
        assert!(!text.contains("configuration"), "{text}");
    }
    // A key over plain http to a non-loopback host is still refused.
    let keyed = Run::anthropic(dir)
        .env("KILN_TEST_KEY", SENTINEL_KEY)
        .arg("--prompt", "hi")
        .arg("--base-url", "http://host.lima.internal:8788")
        .output();
    refused(dir, &keyed, "requires an https base URL");
}
