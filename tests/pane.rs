//! The Herdr pane. `herdr` is always a fake script that records its arguments,
//! so no real pane is ever opened here.

mod common;

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::json;

use common::{
    base_handler, cli, code, fake_herdr, idle, lines, notify, ok, start_mock, thread_value,
    Recorder, TempDir,
};

/// A mock that starts thread `T1`, answers one turn, resumes and steers it,
/// plus the fake herdr.
struct Fixture {
    port: u16,
    state: TempDir,
    herdr: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        Self::with_turn(tag, false)
    }

    /// `completes` is whether the mock ends the turn as soon as it starts.
    fn with_turn(tag: &str, completes: bool) -> Self {
        let recorder = Recorder::default();
        let port = start_mock(base_handler(
            recorder,
            "T1",
            idle(),
            move |msg, tx, method| match method {
                "turn/start" => {
                    ok(tx, msg, json!({ "turn": { "id": "U1" } }));
                    if completes {
                        notify(
                            tx,
                            "turn/completed",
                            json!({
                                "threadId": "T1",
                                "turn": { "id": "U1", "status": "completed" }
                            }),
                        );
                    }
                }
                "thread/resume" => ok(
                    tx,
                    msg,
                    json!({ "thread": thread_value("T1", idle(), json!([])) }),
                ),
                "turn/steer" => ok(tx, msg, json!({})),
                _ => {}
            },
        ));
        let state = TempDir::new(tag);
        let herdr = fake_herdr(&state);
        Self { port, state, herdr }
    }

    /// `run --no-wait` inside a Herdr session, with `agent get` answering `get`.
    fn run(&self, agent_get: &str, extra: &[(&str, &str)]) -> Output {
        self.command(agent_get, extra)
            .args(["run", "--cwd", "/tmp", "--prompt", "hi", "--no-wait"])
            .output()
            .expect("running agent-bridge")
    }

    /// Any verb inside a Herdr session, with `agent get` answering `get`.
    fn command(&self, agent_get: &str, extra: &[(&str, &str)]) -> Command {
        let mut command = cli(self.port, &self.state);
        command
            .env("HERDR_ENV", "1")
            .env("AGENT_BRIDGE_CODEX_HERDR_BIN", &self.herdr)
            .env("AGENT_BRIDGE_CODEX_PANE_RETRY_MS", "50")
            .env("FAKE_HERDR_LOG", self.state.join("herdr-args.txt"))
            .env(
                "FAKE_HERDR_RENAME_COUNT",
                self.state.join("rename-count.txt"),
            )
            .env("FAKE_HERDR_AGENT_GET", agent_get);
        for (key, value) in extra {
            command.env(key, value);
        }
        command
    }

    fn herdr_log(&self) -> Option<String> {
        std::fs::read_to_string(self.state.join("herdr-args.txt")).ok()
    }
}

/// A `herdr pane list` answer: one pane per `(pane_id, label)`, plus one that
/// carries no label at all, the way herdr omits the field for unnamed panes.
fn pane_list(panes: &[(&str, &str)]) -> String {
    let mut listed = vec![json!({ "pane_id": "PX" })];
    listed.extend(
        panes
            .iter()
            .map(|(id, label)| json!({ "pane_id": id, "label": label })),
    );
    json!({ "result": { "type": "pane_list", "panes": listed } }).to_string()
}

/// A `herdr pane process-info` answer with one foreground process.
fn process_info(argv: &[&str]) -> String {
    json!({ "result": { "type": "pane_process_info", "process_info": {
        "foreground_processes": [{ "name": argv[0], "argv": argv }]
    } } })
    .to_string()
}

/// How many recorded invocations start with `prefix`.
fn calls(log: &str, prefix: &str) -> usize {
    log.lines().filter(|l| l.trim().starts_with(prefix)).count()
}

/// The stderr lines that are about the pane.
fn pane_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|l| l.contains("herdr pane"))
        .map(str::to_string)
        .collect()
}

/// `run` must print exactly the started event and exit 0, pane or no pane.
fn assert_started(output: &Output) {
    assert_eq!(code(output), 0);
    let lines = lines(output);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["event"], "started");
    assert_eq!(lines[0]["threadId"], "T1");
}

#[test]
fn a_pane_is_opened_when_no_agent_is_attached_yet() {
    let fixture = Fixture::new("pane-open");
    let output = fixture.run("1", &[]);
    assert_started(&output);
    assert_eq!(
        pane_lines(&output),
        Vec::<String>::new(),
        "a pane that came up is no news"
    );

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent get codex-T1"), "got: {log}");
    assert!(
        log.contains("pane split --current --direction right --ratio 0.45 --cwd /tmp"),
        "got: {log}"
    );
    assert!(
        log.contains(&format!(
            "agent start codex-T1 --kind codex --pane P9 --timeout 5000 -- resume T1 --remote ws://127.0.0.1:{}",
            fixture.port
        )),
        "got: {log}"
    );
    assert!(
        log.contains("pane rename P9 codex-T1"),
        "the pane carries the name even after the TUI goes: {log}"
    );
    assert!(
        log.contains("agent rename P9 codex-T1"),
        "the name is bound to the agent in the pane: {log}"
    );
    assert_eq!(calls(&log, "agent rename"), 1, "one rename suffices: {log}");
    assert!(
        log.find("agent start") < log.find("agent rename"),
        "herdr renames nothing whose start is still pending: {log}"
    );
}

#[test]
fn a_start_that_times_out_is_no_news() {
    let fixture = Fixture::new("pane-start-timeout");
    let output = fixture.run("1", &[("FAKE_HERDR_START_FAIL", "timeout")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent rename P9 codex-T1"), "got: {log}");
    assert_eq!(
        pane_lines(&output),
        Vec::<String>::new(),
        "herdr never sees this TUI as ready; the agent being there is what counts"
    );
}

#[test]
fn a_pane_showing_the_thread_only_gets_its_agent_name() {
    let fixture = Fixture::new("pane-rebind");
    let listed = pane_list(&[("P4", "codex-T1")]);
    let info = process_info(&["codex", "resume", "T1", "--remote", "ws://127.0.0.1:1"]);
    let output = fixture.run(
        "1",
        &[
            ("FAKE_HERDR_PANE_LIST", listed.as_str()),
            ("FAKE_HERDR_PROCESS_INFO", info.as_str()),
            ("FAKE_HERDR_AGENT_PANE", "P4"),
        ],
    );
    assert_started(&output);
    assert_eq!(pane_lines(&output), Vec::<String>::new());

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("pane process-info --pane P4"), "got: {log}");
    assert!(
        log.contains("agent rename P4 codex-T1"),
        "the TUI is there and only lacks its name: {log}"
    );
    assert!(
        !log.contains("pane split") && !log.contains("agent start"),
        "nothing is opened beside it: {log}"
    );
    assert!(
        !log.contains("--clear"),
        "and the pane keeps its label: {log}"
    );
}

#[test]
fn a_pane_showing_anything_else_gives_up_the_name_and_is_left_alone() {
    let cases = [
        ("pane-stale-shell", process_info(&["pwsh"])),
        (
            "pane-stale-other",
            process_info(&["codex", "resume", "T2", "--remote", "ws://127.0.0.1:1"]),
        ),
        // herdr had nothing to say about the pane at all.
        ("pane-stale-unknown", String::new()),
    ];
    for (tag, info) in cases {
        let fixture = Fixture::new(tag);
        let listed = pane_list(&[("P4", "codex-T1")]);
        let output = fixture.run(
            "1",
            &[
                ("FAKE_HERDR_PANE_LIST", listed.as_str()),
                ("FAKE_HERDR_PROCESS_INFO", info.as_str()),
            ],
        );
        assert_started(&output);
        assert_eq!(pane_lines(&output), Vec::<String>::new(), "{tag}");

        let log = fixture.herdr_log().expect("herdr must have been called");
        assert!(
            log.contains("pane rename P4 --clear"),
            "{tag}: the old pane must not keep the name: {log}"
        );
        assert!(
            !log.contains("--pane P4 --timeout") && !log.contains("agent rename P4"),
            "{tag}: nothing is typed into it or bound to it: {log}"
        );
        assert!(log.contains("pane split --current"), "{tag}: {log}");
        assert!(log.contains("pane rename P9 codex-T1"), "{tag}: {log}");
        assert!(
            log.contains("agent start codex-T1 --kind codex --pane P9"),
            "{tag}: {log}"
        );
        assert!(log.contains("agent rename P9 codex-T1"), "{tag}: {log}");
    }
}

#[test]
fn a_pane_list_without_the_name_still_splits() {
    let fixture = Fixture::new("pane-list-miss");
    let listed = pane_list(&[("P4", "codex-OTHER")]);
    let output = fixture.run("1", &[("FAKE_HERDR_PANE_LIST", listed.as_str())]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("pane list"), "got: {log}");
    assert!(
        !log.contains("process-info") && !log.contains("--clear"),
        "another thread's pane is none of this one's business: {log}"
    );
    assert!(
        log.contains("pane split --current"),
        "no pane answers to the name, so one is split: {log}"
    );
    assert!(log.contains("pane rename P9 codex-T1"), "got: {log}");
    assert!(log.contains("agent start codex-T1"), "got: {log}");
}

#[test]
fn a_rename_that_fails_at_first_is_retried() {
    let fixture = Fixture::new("pane-rename-retry");
    let output = fixture.run("1", &[("FAKE_HERDR_RENAME_FAILS", "3")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert_eq!(
        calls(&log, "agent rename"),
        4,
        "three failures, then the one that sticks: {log}"
    );
    assert_eq!(
        pane_lines(&output),
        Vec::<String>::new(),
        "a rename that eventually works says nothing"
    );
}

#[test]
fn a_name_that_never_binds_is_reported_once() {
    let fixture = Fixture::new("pane-rename-fail");
    let output = fixture.run("1", &[("FAKE_HERDR_RENAME_FAILS", "99")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert_eq!(calls(&log, "agent rename"), 10, "ten attempts: {log}");
    let reported = pane_lines(&output);
    assert_eq!(reported.len(), 1, "exactly one line about it: {reported:?}");
    assert!(
        reported[0].contains("herdr agent rename P9 codex-T1 failed 10 times"),
        "got: {reported:?}"
    );
    assert!(
        !reported[0].contains("agent start"),
        "the start went through, so it has nothing to add: {reported:?}"
    );
}

#[test]
fn a_start_herdr_refused_is_part_of_that_report() {
    let fixture = Fixture::new("pane-start-refused");
    let output = fixture.run(
        "1",
        &[
            ("FAKE_HERDR_START_FAIL", "1"),
            ("FAKE_HERDR_RENAME_FAILS", "99"),
        ],
    );
    assert_started(&output);

    let reported = pane_lines(&output);
    assert_eq!(reported.len(), 1, "got: {reported:?}");
    assert!(
        reported[0].contains("herdr agent rename P9 codex-T1 failed 10 times")
            && reported[0].contains("herdr agent start: start refused"),
        "got: {reported:?}"
    );
}

#[test]
fn an_existing_agent_is_left_alone() {
    let fixture = Fixture::new("pane-reuse");
    let output = fixture.run("0", &[]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent get codex-T1"), "got: {log}");
    assert_eq!(
        log.lines().count(),
        1,
        "the agent answers to the name, and that settles it: {log}"
    );
}

#[test]
fn a_failing_pane_changes_neither_stdout_nor_the_exit_code() {
    let fixture = Fixture::new("pane-fail");
    let output = fixture.run("1", &[("FAKE_HERDR_SPLIT_FAIL", "1")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("pane split"), "got: {log}");
    assert!(
        !log.contains("agent start"),
        "the split failed, so nothing is attached: {log}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pane split failed"),
        "the failure belongs on stderr"
    );
}

#[test]
fn a_waiting_run_brings_the_pane_up_beside_the_turn() {
    let fixture = Fixture::with_turn("pane-waiting", true);
    let output = fixture
        .command("1", &[])
        .args(["run", "--cwd", "/tmp", "--prompt", "hi"])
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "{output:?}");
    let events = lines(&output);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["event"], "started");
    assert_eq!(events[1]["event"], "turn");
    assert_eq!(pane_lines(&output), Vec::<String>::new());

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent rename P9 codex-T1"), "got: {log}");
}

#[test]
fn a_waiting_run_still_reports_a_pane_that_outlasted_the_turn() {
    let fixture = Fixture::with_turn("pane-waiting-fail", true);
    let output = fixture
        .command("1", &[("FAKE_HERDR_RENAME_FAILS", "99")])
        .args(["run", "--cwd", "/tmp", "--prompt", "hi"])
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "the pane never decides that: {output:?}");
    assert_eq!(lines(&output)[1]["event"], "turn");

    let reported = pane_lines(&output);
    assert_eq!(reported.len(), 1, "got: {reported:?}");
    assert!(
        reported[0].contains("herdr agent rename P9 codex-T1 failed 10 times"),
        "the turn was over well before the last attempt: {reported:?}"
    );
}

#[test]
fn no_pane_skips_it() {
    let fixture = Fixture::new("pane-off");
    let mut command = cli(fixture.port, &fixture.state);
    let output = command
        .env("HERDR_ENV", "1")
        .env("AGENT_BRIDGE_CODEX_HERDR_BIN", &fixture.herdr)
        .env("FAKE_HERDR_LOG", fixture.state.join("herdr-args.txt"))
        .env("FAKE_HERDR_AGENT_GET", "1")
        .args([
            "--no-pane",
            "run",
            "--cwd",
            "/tmp",
            "--prompt",
            "hi",
            "--no-wait",
        ])
        .output()
        .expect("running agent-bridge");

    assert_started(&output);
    assert_eq!(fixture.herdr_log(), None, "herdr must not be called at all");
}

#[test]
fn outside_herdr_there_is_no_pane() {
    let fixture = Fixture::new("pane-outside");
    let mut command = cli(fixture.port, &fixture.state);
    // `cli` already removes HERDR_ENV; this is the case where nobody set it.
    let output = command
        .env("AGENT_BRIDGE_CODEX_HERDR_BIN", &fixture.herdr)
        .env("FAKE_HERDR_LOG", fixture.state.join("herdr-args.txt"))
        .env("FAKE_HERDR_AGENT_GET", "1")
        .args(["run", "--cwd", "/tmp", "--prompt", "hi", "--no-wait"])
        .output()
        .expect("running agent-bridge");

    assert_started(&output);
    assert_eq!(fixture.herdr_log(), None, "herdr must not be called at all");
}

#[test]
fn steer_opens_a_pane_like_run() {
    let fixture = Fixture::new("pane-steer");
    let output = fixture
        .command("1", &[])
        .args(["steer", "--thread", "T1", "--turn", "U1", "--text", "go"])
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(lines(&output)[0]["event"], "steered");

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(
        log.contains("pane split --current --direction right --ratio 0.45 --cwd /tmp"),
        "steer sends the agent input, so a human can watch: {log}"
    );
    assert!(log.contains("agent start codex-T1"), "got: {log}");
}

#[test]
fn a_steer_that_is_refused_opens_no_pane() {
    let fixture = Fixture::new("pane-steer-refused");
    // No `--turn`, and the mock's thread has no turn in progress to find.
    let output = fixture
        .command("1", &[])
        .args(["steer", "--thread", "T1", "--text", "go"])
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 4, "{output:?}");
    assert_eq!(
        fixture.herdr_log(),
        None,
        "the message goes first, and it never went"
    );
}

#[test]
fn wait_and_reply_never_open_a_pane() {
    let fixture = Fixture::new("pane-attach-only");
    let verbs: [&[&str]; 3] = [
        &["wait", "--thread", "T1"],
        // Kept as a harmless flag for callers that still pass it.
        &["wait", "--thread", "T1", "--no-pane"],
        &[
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "5",
            "--decision",
            "accept",
        ],
    ];
    for args in verbs {
        let output = fixture
            .command("1", &[])
            .args(args)
            .output()
            .expect("running agent-bridge");
        assert_eq!(code(&output), 0, "{args:?}: {output:?}");
        assert_eq!(lines(&output)[0]["event"], "turn", "{args:?}");
        assert_eq!(
            fixture.herdr_log(),
            None,
            "{args:?} must not call herdr at all"
        );
    }
}
