//! Processes hail started (`emulator.launch`, automount launches, `process.run`), kept so
//! they can be stopped later. Those still running when hail exits are stopped then
//! ([`crate::Service::shutdown`]).
use crate::emulator::{CloseRequest, Process, Started};
use crate::event::utc_timestamp;
use crate::log::EventLog;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often stopping processes are checked for exit.
const STOP_POLL: Duration = Duration::from_millis(25);
/// How long a killed process may take to disappear.
const KILL_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessKind {
    /// Started by `emulator.launch` or `emulator.automount`.
    Emulator,
    /// Started by `process.run`.
    Process,
}

impl ProcessKind {
    pub fn name(self) -> &'static str {
        match self {
            ProcessKind::Emulator => "emulator",
            ProcessKind::Process => "process",
        }
    }
}

/// A running process hail started.
#[derive(Debug)]
pub struct TrackedProcess {
    pub kind: ProcessKind,
    pub name: String,
    pub pid: u32,
    /// Session/account it runs in.
    pub session: String,
    pub started_at: String,
    /// Mount point of the disk whose `usb.inserted` chain started it.
    pub disk: Option<PathBuf>,
    /// Stopped when that disk is removed.
    pub stop_on_remove: bool,
    process: Process,
}

impl TrackedProcess {
    /// True for `kind` and `name` (ignoring case), or any of `kind` when `name` is `*`.
    pub fn matches(&self, kind: ProcessKind, name: &str) -> bool {
        self.kind == kind && (name == "*" || self.name.eq_ignore_ascii_case(name))
    }

    fn label(&self) -> String {
        format!("{} (pid {})", self.name, self.pid)
    }
}

/// The processes hail started that are still running.
#[derive(Debug, Default)]
pub struct ProcessTable {
    entries: Vec<TrackedProcess>,
}

fn exit_text(code: Option<i32>) -> String {
    code.map_or("no exit code (signal)".to_string(), |code| {
        format!("exit code {code}")
    })
}

impl ProcessTable {
    pub fn entries(&self) -> &[TrackedProcess] {
        &self.entries
    }

    pub fn add(
        &mut self,
        kind: ProcessKind,
        name: &str,
        started: Started,
        disk: Option<&Path>,
        stop_on_remove: bool,
    ) {
        self.entries.push(TrackedProcess {
            kind,
            name: name.to_string(),
            pid: started.pid,
            session: started.session,
            started_at: utc_timestamp(SystemTime::now()),
            disk: disk.map(Path::to_path_buf),
            stop_on_remove,
            process: started.process,
        });
    }

    /// The first running process of `kind` named `name` (ignoring case).
    pub fn find(&self, kind: ProcessKind, name: &str) -> Option<&TrackedProcess> {
        self.entries.iter().find(|entry| entry.matches(kind, name))
    }

    /// Forgets processes that exited on their own, logging their exit codes.
    pub fn reap(&mut self, log: &mut EventLog) -> Result<()> {
        let mut index = 0;
        while index < self.entries.len() {
            match self.entries[index].process.try_exit() {
                None => index += 1,
                Some(code) => {
                    let entry = self.entries.remove(index);
                    log.write_line(&format!(
                        "{} [{}] {} exited ({})",
                        utc_timestamp(SystemTime::now()),
                        entry.kind.name(),
                        entry.label(),
                        exit_text(code)
                    ))?;
                }
            }
        }
        Ok(())
    }

    /// Stops the selected processes: each is asked to close (window close / SIGTERM),
    /// then those still running after `timeout` are killed. Every outcome is logged with
    /// `reason`. Returns how many were stopped. A log write failure does not stop the
    /// others from being stopped; the first one is returned at the end.
    pub fn stop(
        &mut self,
        log: &mut EventLog,
        select: impl Fn(&TrackedProcess) -> bool,
        timeout: Duration,
        reason: &str,
    ) -> Result<usize> {
        let mut logged = self.reap(log);
        let (mut stopping, kept): (Vec<_>, Vec<_>) =
            self.entries.drain(..).partition(|entry| select(entry));
        self.entries = kept;
        let start = Instant::now();
        // (entry, how it was asked, outcome once known)
        let mut states: Vec<(TrackedProcess, String, Option<String>)> = stopping
            .drain(..)
            .map(|mut entry| match entry.process.request_close() {
                CloseRequest::Sent(how) => (entry, how, None),
                CloseRequest::Unsupported(why) => {
                    let outcome = match entry.process.kill() {
                        Ok(()) => None,
                        Err(error) => Some(format!("{why}; terminate failed: {error}")),
                    };
                    (entry, format!("{why}; terminated"), outcome)
                }
            })
            .collect();
        let deadline = start + timeout;
        loop {
            let mut pending = false;
            for (entry, how, outcome) in states.iter_mut().filter(|state| state.2.is_none()) {
                match entry.process.try_exit() {
                    Some(code) => {
                        *outcome = Some(format!(
                            "{how}; exited after {} ms ({})",
                            start.elapsed().as_millis(),
                            exit_text(code)
                        ))
                    }
                    None => pending = true,
                }
            }
            if !pending || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(STOP_POLL);
        }
        for (entry, how, outcome) in states.iter_mut().filter(|state| state.2.is_none()) {
            let killed = entry.process.kill();
            let kill_deadline = Instant::now() + KILL_WAIT;
            let code = loop {
                if let Some(code) = entry.process.try_exit() {
                    break Some(code);
                }
                if Instant::now() >= kill_deadline {
                    break None;
                }
                std::thread::sleep(STOP_POLL);
            };
            *outcome = Some(match (killed, code) {
                (_, Some(code)) => format!(
                    "{how}; still running after {} ms, killed ({})",
                    timeout.as_millis(),
                    exit_text(code)
                ),
                (Err(error), None) => format!("{how}; kill failed: {error}"),
                (Ok(()), None) => format!("{how}; killed but still running"),
            });
        }
        let count = states.len();
        for (entry, _, outcome) in states {
            let written = log.write_line(&format!(
                "{} [{}] stopped {} ({reason}): {}",
                utc_timestamp(SystemTime::now()),
                entry.kind.name(),
                entry.label(),
                outcome.unwrap_or_default()
            ));
            logged = logged.and(written);
        }
        logged.map(|()| count)
    }
}
