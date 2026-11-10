# Threat boundaries

The model, its arguments, tool results, sandbox output, and files are untrusted.
The host operator, kilnd socket owner/group, harness binary, and configured
provider credential source are trusted. A configured model endpoint is
trusted for availability only: it can stall or refuse a session, but every
response it sends is untrusted input that is bounded and validated before it
reaches the loop, and what the model asks for is checked exactly as before. Anyone who can reach the kilnd socket
has near-root sandbox-control capability on this development node.

kiln supplies namespace, cgroup, quota, seccomp, Landlock, and network
isolation. It shares the host kernel, is experimental, and has not been audited;
it is not a hostile multi-tenant boundary.

## Enforced here

- Only `kilnd.v1` is used. Production code never starts `kiln`, reads its
  filesystem state, or runs a host shell.
- Every session requests network none. The harness itself talks to the model
  endpoint from the host; the sandbox never gets a network or a credential.
- Typed JSON arguments reject unknown fields. File helpers accept only
  `/workspace` paths, relative or with the `/workspace/` prefix stripped
  once, and refuse every other absolute path and any `.` or `..` component; `exec` remains a deliberately general sandbox
  argv primitive.
- Step, call, argv, stdin, file, manifest, result, per-execution, and session
  bounds are finite. kilnd supplies a second validation and capture boundary.
- Provider, audit, budget, cancellation, deadline, and tool errors all enter a
  separate bounded cleanup path. Every delete acknowledgement and ambiguous
  error is independently checked with a complete non-deleted listing. Delete
  and verification are retried within the same cleanup window; failed or
  stalled verification is surfaced as unconfirmed cleanup.
- Create recovery classifies typed gRPC status codes: invalid argument, resource
  exhausted, failed precondition, permission denied, and unauthenticated are
  definite rejections only after successful recovery listings. Other codes and
  local transport/protocol failures remain ambiguous without a recovered row.
- Audit records omit content. They store hashes, byte/truncation evidence,
  token usage, host-generated call correlation IDs, sandbox IDs, and kilnd
  execution IDs. Raw provider call IDs stay out of the audit.
- The session journal deliberately contains content: prompts, model text, tool
  arguments, tool results, and final answers, because replay needs them. It is
  a trusted host-side file created new with mode 0600 and is never placed in or
  mounted into a sandbox. Readers and writers bound file and line sizes, and
  writers never buffer a record past the line limit.
- The journal and audit directories must be operator-owned, and the harness
  enforces this through pinned descriptors: every directory from `/` down is
  opened without following links (so it must be readable and searchable by
  the operator) and checked by `fstat`, the final one belongs to the effective
  user and is writable by no one else, every ancestor belongs to root or that
  user and is sticky if others can write it, and the file is created relative
  to the verified directory. On Linux, POSIX ACL entries for named users and
  groups show in the group mode bits as the ACL mask and are refused with
  them, and the explicit modes of created files and directories reset the
  mask, capping an inherited default ACL. On macOS the directory rule checks
  ownership and mode bits only; extended ACLs are not inspected. Do not place
  journals under a directory with inheritable ACLs. Journal sources are opened
  without following links and non-blocking, then required to be regular files
  by `fstat`. `replay` holds its source open read-only and refuses any output
  whose open descriptor is the same file as the source or the other output,
  so relinking a path after the checks cannot redirect a write. A
  group-writable directory is refused even when its group is the user's own:
  on macOS that group is the shared `staff`. Directories and files the harness
  creates get 0700 and 0600 explicitly, whatever the umask. A failed or
  refused open removes what it created through the pinned parent
  descriptors, checking with `fstatat` that each entry is still the one it
  made and empty. A name could change between that check and `unlinkat`, but
  only by someone who can write the parent, which the walk verified is only
  the operator.
- Provider credentials stay host-side and are never journaled or audited.
  `run` reads a key only from the environment variable named by
  `--api-key-env` (never a flag, so never in argv), once at startup, and
  refuses an empty one. It is held in a `SecretString` whose `Debug` is
  redacted and which cannot be displayed or serialized, and is used only to
  build one header, marked sensitive: `Authorization` for openai-compat,
  `x-api-key` for anthropic, which always has a key. Nothing puts it in
  `ModelRequest` or `ModelResponse`. The endpoint receives the key, so its
  responses may echo it: with a key, error bodies are never quoted (the
  message is the status and `body withheld: authenticated request`), and a
  success is refused whole as `response contained the request credential`
  when the key, exactly or in its JSON-escaped spelling, appears at any JSON
  layer the protocol defines: the raw body; every decoded string and object
  key; the parsed text, call IDs, and tool names; and the JSON nested inside
  `function.arguments`, decoded whatever its type (an object, string, array,
  number, or null), as well as the raw string kept when it is not an
  object. The claim is
  therefore exact: the harness never writes the key, or a JSON-escaped form
  of it, to the journal, the audit, or an error. An endpoint that holds the
  key can still return a transformed or split form of it (base64, reversed,
  one half per field), and that cannot be detected.
- Anthropic's opaque content blocks (`thinking`, `redacted_thinking`,
  `fallback`, and unknown types) are untrusted endpoint data like any other
  response: bounded by the response cap, scanned for the key at every
  decoded layer, counted against `max_total_model_output_bytes`, and never
  interpreted. They reach the next request unchanged and the journal as
  recorded, but never a tool, the audit, or the terminal. On current models
  a `thinking` block holds no chain-of-thought text by default (its
  `thinking` field is empty unless a summary is requested, which this
  adapter does not do), only a signature. The served model name is the one
  endpoint string the audit carries; it must be 1..=256 bytes of letters,
  digits, and `._:@/-` or the response is refused.
- Plain `http://` to a model server on the LAN is acceptable for local models
  without a key: the traffic is prompts and tool results the operator already
  sees, and a hostile LAN peer can at worst feed the loop responses, which
  are untrusted anyway. A key requires `https` unless the base URL's host is a
  loopback IP literal; host names, `localhost` included, are not treated as
  loopback. The client follows no redirects and ignores proxy environment
  variables, so the key and the transcript go only to the configured
  endpoint. Base URLs with userinfo, a query, or a fragment are refused.
- Provider responses are bounded in decoded HTTP payload bytes: requests over
  `max_request_bytes` are not sent, response bodies are read only up to
  `max_response_bytes` (4 MiB by default, 64 MiB at most) with no
  decompression, error bodies to 64 KiB, and unauthenticated error messages
  quote at most 512 bytes, without control characters. Transfer framing
  (chunk headers and extensions, HTTP/2 frames) is below that count; it is
  bounded only by the session deadline and cancellation, and by hyper's own
  limits, such as 16 KiB of chunk extensions per message. Malformed
  responses, including one without token usage or with plainly wrong usage,
  fail the session instead of being guessed at.
- Reported token usage is an endpoint property the harness relies on for
  cost accounting, not for safety. Usage that cannot be right (zero prompt
  tokens, zero completion tokens for non-empty output, or more than 64 bytes
  of output per completion token) is refused, but a plausible lie is not
  detectable without the model's tokenizer. Size is bounded locally
  regardless: `max_total_model_output_bytes` (8 MiB by default, 256 MiB at
  most) caps the text, call IDs, tool names, and tool-call arguments a
  session accepts, counted by the harness, and the journal grammar applies
  the same budget. Tool names over 256 bytes are refused before they are
  kept, and call IDs are kept only when at most 64 bytes.
- Model text printed by `run`, `demo`, and `replay` has C0 and C1 control
  characters and DEL (other than newline and tab) shown as `\u{..}`, so a
  final answer cannot drive the operator's terminal. The journal keeps the
  raw text.
- `--prompt-file` and `--system-file` are opened like journal sources:
  without following a final symlink and non-blocking, then required to be
  regular files by `fstat` and read only up to the task limit, so a FIFO
  cannot stall `run` before its deadline exists and a symlink is refused.
- Every response is bounded before it reaches kilnd: per-turn and per-session
  call budgets, per-request and cumulative token budgets (cached input
  included), argument and
  assistant-text byte limits, and fail-closed handling of truncated or refused
  turns. Malformed or repeated provider call IDs fail the session; unregistered
  tool names only produce a model-visible `unknown_tool` result. Configured and
  recorded limits are capped by fixed ceilings before any sandbox exists.

## Deliberate limits

- Lexical workspace validation is not a filesystem capability system. A
  symlink under `/workspace` can target another sandbox path; read-only root
  blocks helper writes outside approved writable paths, and the sandbox—not the
  workspace prefix—is the host boundary. General `exec` can read any path the
  sandbox policy permits.
- A model endpoint is trusted for availability only. A slow or hanging server
  delays the session until its deadline, and retries (at most 8 attempts,
  each backoff capped) spend session time, not extra budget. Dropping the
  provider future at the deadline or on cancellation closes the HTTP
  connection; the server may still finish generating.
- Cancellation races active provider and tool RPCs. Dropping an Exec RPC asks
  kilnd to cancel its daemon-owned execution; sandbox deletion then provides
  the cleanup barrier. Create response loss is recovered by unique session
  labels. Lifecycle recovery may outlive the work deadline; its default is 10
  seconds and configurations above 60 seconds are rejected. Exhaustion is an
  explicit `Cleanup` or `CreateUncertain` error.
- SIGKILL, host loss, or a harness crash can prevent client-side delete. kilnd
  reconciliation handles known state; a future sandbox TTL is still needed for
  unattended client crashes.
- The audit JSONL file is append-only by open mode, not tamper-proof. A trusted
  host user can modify it. Durable signing or a remote append service is out of
  scope.
- The journal's hash chain detects accidental corruption, reordering, and
  dropped middle records. It is not tamper-proof: anyone who can write the file
  can rewrite the chain. A missing tail shows only as `complete: false`, and a
  torn final line is rejected. The event grammar binds every tool call to the
  model's sanitized call and every result that fed a later request to that
  request's hash, and rejects impossible orders and terminal records that
  contradict the session; a rewriter who forges a consistent session,
  including its requests, is not detected. Results of a final tool turn are
  bound only by replay. The grammar also applies the recorded limits as the
  engine does, so a journal cannot continue past a step, call, token,
  model-output byte, or text budget, or past a refused response.
- Replay trusts the journal it reads, but replayed responses pass through the
  same loop checks and budgets as live ones. It reproduces model responses,
  not sandbox state, and detects divergence by comparing each tool call and
  result with the recording and by SHA-256 over request content.
- kilnd persists bounded command output in its root-only SQLite database. The
  harness audit excludes it, but callers must still avoid printing host secrets
  into a sandbox command.
