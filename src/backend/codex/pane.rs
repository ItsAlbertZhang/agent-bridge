//! The Herdr pane: a human-visible Codex TUI attached to the thread.
//!
//! Best effort by construction. Every failure is one line on stderr and nothing
//! else: the pane never touches stdout or the exit code. Nothing here types
//! into a pane it did not split itself, and nothing closes a pane either — the
//! human presses Ctrl+C in the TUI and closes it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// How long herdr may spend on `agent start`, which is how long a pane takes to
/// come up: herdr cannot tell when this TUI is ready, so every start runs into
/// this timeout (see [`start`]). herdr takes nothing under three seconds.
const START_TIMEOUT_MS: &str = "5000";

/// Pause between `agent rename` attempts, and how many there are. herdr renames
/// only an agent it has detected in the pane, and none whose start is still
/// pending, which another call's may be.
const RENAME_RETRY_MS: u64 = 1_000;
const RENAME_ATTEMPTS: u32 = 10;

/// The herdr executable, when a pane is wanted at all.
///
/// `None` means "no pane": `--no-pane`, not running inside Herdr, or no herdr
/// executable to call. `AGENT_BRIDGE_CODEX_HERDR_BIN` substitutes a fake in tests.
pub fn herdr_bin(no_pane: bool) -> Option<PathBuf> {
    if no_pane || std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
        return None;
    }
    if let Ok(explicit) = std::env::var("AGENT_BRIDGE_CODEX_HERDR_BIN") {
        return (!explicit.is_empty()).then(|| PathBuf::from(explicit));
    }
    which("herdr")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions.iter().find_map(|ext| {
            let candidate = dir.join(format!("{name}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Herdr names, for the pane and the agent alike, are `codex-` plus the last 8
/// of the 12 hex digits a thread id opens with. Those 12 are the thread's
/// creation time in milliseconds, and their first 8 stay the same for about a
/// minute, which would hand threads started together the same name.
pub fn agent_name(thread_id: &str) -> String {
    let stamp: Vec<char> = thread_id.chars().filter(|c| *c != '-').take(12).collect();
    let low = &stamp[stamp.len().saturating_sub(8)..];
    format!("codex-{}", low.iter().collect::<String>())
}

fn rename_retry_ms() -> u64 {
    std::env::var("AGENT_BRIDGE_CODEX_PANE_RETRY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENAME_RETRY_MS)
}

/// One line on stderr, which is all any pane trouble ever earns.
fn warn(message: &str) {
    eprintln!("agent-bridge codex: herdr pane: {message}");
}

async fn herdr<I, S>(bin: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {}", bin.display()))
}

/// Open a pane for `thread_id`, unless the thread is being watched already.
///
/// Three ways it can go: an agent answers to the thread's name and nothing is
/// needed; a pane carries the name and runs the thread's TUI, which only lacks
/// its agent name, so that is bound; or a pane is split and codex started in it.
pub async fn open(bin: &Path, thread_id: &str, cwd: &str, ws_url: &str) -> Result<()> {
    let name = agent_name(thread_id);
    if herdr(bin, ["agent", "get", &name]).await?.status.success() {
        return Ok(()); // Someone is already watching this thread.
    }

    // The agent name dies with the TUI, the pane label does not, so a pane can
    // carry the name with anything at all running in it.
    if let Some(pane_id) = pane_by_label(bin, &name).await {
        if shows_thread(bin, &pane_id, thread_id).await {
            return rename(bin, &pane_id, &name).await;
        }
        // Not this thread's TUI: a shell the TUI left behind, or whatever the
        // human started there since. Nothing is typed into such a pane, and it
        // gives up the name the new pane is about to take.
        //
        // Known gap: a pane split for this thread a moment ago looks just like
        // that for about a second, until codex is its foreground process. A
        // second call for the thread that lands in that second splits a pane
        // of its own, and the thread ends up with two TUIs.
        if let Err(err) = clear_label(bin, &pane_id).await {
            warn(&format!("{err:#}"));
        }
    }

    let pane_id = split(bin, cwd).await?;
    // Label the pane straight away: this is what the next call finds while the
    // agent in it has no name yet.
    if let Err(err) = label_pane(bin, &pane_id, &name).await {
        warn(&format!("{err:#}"));
    }

    // What the start said only matters when no agent turns up to take the name.
    let started = start(bin, &pane_id, &name, thread_id, ws_url).await;
    let Err(err) = rename(bin, &pane_id, &name).await else {
        return Ok(());
    };
    match started {
        Ok(started) if !started.status.success() => bail!(
            "{err:#}; herdr agent start: {}",
            String::from_utf8_lossy(&started.stderr).trim()
        ),
        Ok(_) => Err(err),
        Err(start_err) => bail!("{err:#}; herdr agent start: {start_err:#}"),
    }
}

/// Split a pane beside the caller's.
async fn split(bin: &Path, cwd: &str) -> Result<String> {
    let split = herdr(
        bin,
        [
            "pane",
            "split",
            "--current",
            "--direction",
            "right",
            "--ratio",
            "0.45",
            "--cwd",
            cwd,
        ],
    )
    .await?;
    if !split.status.success() {
        bail!(
            "herdr pane split failed: {}",
            String::from_utf8_lossy(&split.stderr).trim()
        );
    }
    serde_json::from_slice::<Value>(&split.stdout)
        .ok()
        .and_then(|value| {
            value
                .pointer("/result/pane/pane_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("herdr pane split printed no .result.pane.pane_id"))
}

/// Have herdr start codex in `pane_id`, and wait the start out.
///
/// `agent start` answers once the agent takes input or the timeout runs out,
/// and herdr cannot tell when this TUI does: every start ends in that timeout,
/// with codex up in the pane all the same. So the answer says nothing by
/// itself, and [`rename`] working is the proof that the TUI came up. The wait
/// cannot be skipped, though: until the start has returned, herdr refuses to
/// rename the agent, and a start whose client leaves early stays pending.
async fn start(
    bin: &Path,
    pane_id: &str,
    name: &str,
    thread_id: &str,
    ws_url: &str,
) -> Result<Output> {
    herdr(
        bin,
        [
            "agent",
            "start",
            name,
            "--kind",
            "codex",
            "--pane",
            pane_id,
            "--timeout",
            START_TIMEOUT_MS,
            "--",
            "resume",
            thread_id,
            "--remote",
            ws_url,
        ],
    )
    .await
}

/// The id of the pane labelled `name`, if `herdr pane list` reports one.
///
/// Silent by construction: no list, no match and no parse all mean the same
/// thing to the caller, which is "split a pane".
async fn pane_by_label(bin: &Path, name: &str) -> Option<String> {
    let listed = herdr(bin, ["pane", "list"]).await.ok()?;
    if !listed.status.success() {
        return None;
    }
    let value = serde_json::from_slice::<Value>(&listed.stdout).ok()?;
    value
        .pointer("/result/panes")?
        .as_array()?
        .iter()
        .find(|pane| pane.get("label").and_then(Value::as_str) == Some(name))
        .and_then(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_string)
}

/// Whether the pane's foreground process was started on `thread_id`. The TUI
/// is `codex resume <thread id> --remote <url>`, so the id is on its command
/// line, which is there as soon as the process is and does not depend on herdr
/// recognising an agent.
///
/// Silent like [`pane_by_label`]: whatever keeps the answer away reads as "no".
async fn shows_thread(bin: &Path, pane_id: &str, thread_id: &str) -> bool {
    let Ok(info) = herdr(bin, ["pane", "process-info", "--pane", pane_id]).await else {
        return false;
    };
    if !info.status.success() {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&info.stdout) else {
        return false;
    };
    value
        .pointer("/result/process_info/foreground_processes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|process| process.get("argv").and_then(Value::as_array))
        .flatten()
        .any(|arg| arg.as_str() == Some(thread_id))
}

/// Label the pane itself, which outlives the agent attached to it.
async fn label_pane(bin: &Path, pane_id: &str, name: &str) -> Result<()> {
    let output = herdr(bin, ["pane", "rename", pane_id, name]).await?;
    if !output.status.success() {
        bail!(
            "herdr pane rename {pane_id} {name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn clear_label(bin: &Path, pane_id: &str) -> Result<()> {
    let output = herdr(bin, ["pane", "rename", pane_id, "--clear"]).await?;
    if !output.status.success() {
        bail!(
            "herdr pane rename {pane_id} --clear failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Bind `name` to the agent in the pane, retrying until herdr has detected one.
async fn rename(bin: &Path, pane_id: &str, name: &str) -> Result<()> {
    let mut last = String::new();
    for attempt in 0..RENAME_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(rename_retry_ms())).await;
        }
        match herdr(bin, ["agent", "rename", pane_id, name]).await {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => last = String::from_utf8_lossy(&output.stderr).trim().to_string(),
            Err(err) => last = format!("{err:#}"),
        }
    }
    bail!("herdr agent rename {pane_id} {name} failed {RENAME_ATTEMPTS} times: {last}")
}

#[cfg(test)]
mod tests {
    use super::agent_name;

    #[test]
    fn the_agent_name_is_the_low_end_of_the_thread_ids_timestamp() {
        assert_eq!(
            agent_name("0199abcd-ef01-7000-8000-0123456789ab"),
            "codex-abcdef01"
        );
        // Started within the same minute: same first 8 digits, different names.
        assert_ne!(
            agent_name("0199abcd-ef01-7000-8000-0123456789ab"),
            agent_name("0199abcd-f234-7000-8000-0123456789ab")
        );
        assert_eq!(agent_name("short"), "codex-short");
    }
}
