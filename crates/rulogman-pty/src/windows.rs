//! The local pty session driver for Windows, on top of ConPTY.
//!
//! The shape is the unix driver's — see [`crate::session`] — and the public
//! behavior is identical: [`PtySession::spawn`] never blocks, and the event
//! stream is `Ready`, then output, then `Exited` or `Error`. A *control*
//! thread owns the pseudoconsole and writes to it, and a *reader* thread does
//! nothing but blocking reads from a clone of its output handle.
//!
//! ConPTY forces one addition. A unix master reports end of file as soon as
//! the last slave is closed, so the reader hitting EOF *is* the news that the
//! shell exited. A pseudoconsole's output pipe instead stays open for as long
//! as the master lives, whether or not anything is still attached to it, so
//! nothing on the stream ever says "the shell is gone". A third thread — the
//! *waiter* — therefore owns the child and sits in a blocking `wait`, and its
//! return is what ends the session. Winding down then runs in the order the
//! pipes require: kill the child, reap it, close the pseudoconsole (which is
//! what finally breaks the pipe and wakes the reader), and only publish
//! [`PtyEvent::Exited`] once the reader confirms it has drained what the child
//! wrote on its way out.
//!
//! One obligation lands on whoever consumes the event stream. ConPTY opens
//! every session by emitting a device status report — `ESC[6n`, "where is the
//! cursor?" — and holds the child there until the answer comes back as input.
//! That is a question for a terminal emulator, not for a transport: answering
//! it here would mean parsing the output stream, and would put a second reply
//! behind the emulator's own whenever a program asks the same thing later. So
//! the driver passes it on like any other output, and the terminal on top of it
//! — `rulogman-term`, which already replies to device status reports — is what
//! gets the shell moving. A consumer that never answers sees `Ready` and then
//! silence.

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

// The pty, the child and their traits all arrive as boxed trait objects, so
// only the concrete types and the entry point need naming here.
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::config::PtyConfig;
use crate::driver::{Command, EventSink, READ_BUFFER};
use crate::event::PtyEvent;

/// Program started when the configuration names none.
///
/// Windows has neither a login shell nor a `$SHELL` to consult, and PowerShell
/// is what a user opening a terminal on this platform expects. `cmd.exe` would
/// be the other candidate; it is the older, poorer shell of the two.
const DEFAULT_SHELL: &str = "powershell.exe";

/// How long the control thread waits for the reader to confirm it has drained
/// the pipe before publishing `Exited` anyway.
///
/// Only ever spent when something has gone wrong with the pipe: in the normal
/// case closing the pseudoconsole ends the reader's last `read` immediately. A
/// wedged pipe must not strand a session that the user has already closed.
const READER_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// A live (or recently ended) local shell running on its own threads.
///
/// The handle is `Send` and `Sync` and every method is non-blocking, so it can
/// be held and used from a GUI thread. Dropping it ends the session.
pub struct PtySession {
    /// Command channel to the control thread.
    commands: Sender<Command>,
    /// Process ID of the shell while the waiter holds it, `0` otherwise; see
    /// [`PtySession::shell_in_foreground`].
    shell: Arc<AtomicU32>,
}

impl fmt::Debug for PtySession {
    /// Deliberately opaque: the handle is a channel and a promise, and there
    /// is nothing about the shell it can report without asking another thread.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtySession").finish_non_exhaustive()
    }
}

impl PtySession {
    /// Spawns the user's shell on a new pseudoconsole. Never blocks the caller.
    ///
    /// The returned receiver yields every [`PtyEvent`] the session produces,
    /// in order: [`PtyEvent::Ready`] once the shell is running, then its
    /// output, then either [`PtyEvent::Exited`] or — if the shell could not be
    /// started at all — [`PtyEvent::Error`]. The channel closes with that
    /// final event, so a consumer can simply read the stream to its end.
    pub fn spawn(config: PtyConfig) -> (Self, rulogman_stream::Receiver<PtyEvent>) {
        let (event_tx, event_rx) = rulogman_stream::channel(rulogman_stream::OUTPUT_BUDGET);
        let (command_tx, command_rx) = channel();
        let sink = Arc::new(EventSink::new(event_tx));
        let shell = Arc::new(AtomicU32::new(0));

        let session = PtySession {
            commands: command_tx.clone(),
            shell: Arc::clone(&shell),
        };

        let worker_sink = Arc::clone(&sink);
        let spawned = std::thread::Builder::new()
            .name("rulogman-pty".to_owned())
            .spawn(move || control(config, worker_sink, command_rx, command_tx, shell));

        if let Err(error) = spawned {
            sink.close(PtyEvent::Error(format!(
                "could not start the pty worker thread: {error}"
            )));
        }

        (session, event_rx)
    }

    /// Queues `data` for the shell, e.g. keystrokes or pasted text.
    ///
    /// Bytes sent before the shell is ready are queued and written once it is.
    /// Silently ignored after the session has ended.
    pub fn send_input(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let _ = self.commands.send(Command::Input(data));
    }

    /// Tells the shell that the terminal has been resized.
    ///
    /// Reaches the child as a console buffer resize. Silently ignored after
    /// the session has ended.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.commands.send(Command::Resize(cols, rows));
    }

    /// Ends the session. Safe to call more than once.
    ///
    /// The child is killed and reaped, exactly as a closing terminal emulator
    /// would do it.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }

    /// The directory the shell is working in, as the operating system reports
    /// it — which on Windows it does not.
    ///
    /// Present so that the two backends keep one public surface. Windows keeps
    /// a process's working directory in that process's own memory, and reading
    /// it means reading another process's environment block; a shell that
    /// wants its directory known announces it over `OSC 7` instead.
    pub fn current_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Whether the shell has nothing running in front of it, as far as the
    /// process table can tell: whether it has no child process.
    ///
    /// A console has no foreground process group to compare with the shell,
    /// as a unix pty does, so the question is put the one way Windows can
    /// answer it. A program `cmd` or PowerShell starts is a process of its
    /// own, and the shell's child for as long as it runs — so a shell with no
    /// child is at its prompt, or running something of its own. That second
    /// case is the caller's to rule out: a PowerShell script or cmdlet runs
    /// inside the shell's own process, and `wsl.exe` runs everything on the
    /// Linux side, where no Windows process stands for it.
    ///
    /// Unlike the unix answer, a program sent to the background — `start /b`,
    /// a window opened with `notepad` — still counts, because nothing here
    /// says which of its children the shell is waiting for. `None` before the
    /// shell has started, once it has exited, and when the process table
    /// cannot be read.
    pub fn shell_in_foreground(&self) -> Option<bool> {
        match self.shell.load(Ordering::Acquire) {
            0 => None,
            pid => has_child(pid).map(|busy| !busy),
        }
    }
}

impl Drop for PtySession {
    /// A dropped handle must not leave a shell — and its children — running
    /// invisibly for the rest of the process's life.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The control thread: owns the pseudoconsole for as long as the session lives.
fn control(
    config: PtyConfig,
    sink: Arc<EventSink>,
    commands: Receiver<Command>,
    replies: Sender<Command>,
    shell: Arc<AtomicU32>,
) {
    let pair = match native_pty_system().openpty(pty_size(config.cols, config.rows)) {
        Ok(pair) => pair,
        Err(error) => {
            sink.close(PtyEvent::Error(format!(
                "could not open a local pty: {error}"
            )));
            return;
        }
    };

    let mut writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            sink.close(PtyEvent::Error(format!(
                "could not open the local pty for writing: {error}"
            )));
            return;
        }
    };

    // The reader gets its own handle so that reads and writes are independent;
    // both refer to the same pseudoconsole.
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            sink.close(PtyEvent::Error(format!(
                "could not open the local pty for reading: {error}"
            )));
            return;
        }
    };

    let mut child = match pair.slave.spawn_command(command_builder(&config)) {
        Ok(child) => child,
        Err(error) => {
            sink.close(PtyEvent::Error(format!(
                "could not start the local shell: {error}"
            )));
            return;
        }
    };

    // The slave exists only to start the child. Holding on to it would keep a
    // second reference to the pseudoconsole alive and confuse the shutdown
    // below, so it goes as soon as it has done its job.
    drop(pair.slave);
    let master = pair.master;

    // The killer stays with the control thread so that it can end the child at
    // any moment; the child itself goes to the waiter, because the blocking
    // `wait` must not be what the control thread is sitting in.
    let mut killer = child.clone_killer();

    sink.emit(PtyEvent::Ready);

    let reader_sink = Arc::clone(&sink);
    let reader_replies = replies.clone();
    let spawned = std::thread::Builder::new()
        .name("rulogman-pty-reader".to_owned())
        .spawn(move || read_loop(reader, reader_sink, &reader_replies));
    if let Err(error) = spawned {
        // Nothing is parked on the pipe yet, so the child can simply be killed
        // and reaped here rather than handed to a waiter that will not exist.
        let _ = killer.kill();
        let _ = child.wait();
        drop(master);
        sink.close(PtyEvent::Error(format!(
            "could not start the pty reader thread: {error}"
        )));
        return;
    }

    // `replies` is moved into the waiter and cloned into the reader, and the
    // control thread deliberately keeps no copy: a disconnected command channel
    // then means the handle and both workers are gone, which is a shutdown.
    let waiter = std::thread::Builder::new()
        .name("rulogman-pty-waiter".to_owned())
        .spawn(move || {
            // Published from here because the waiter owns the child, and with
            // it the process handle that keeps Windows from handing the ID to
            // another process: withdrawn before that handle goes, a reader
            // never sees an ID that names somebody else.
            if let Some(pid) = child.process_id() {
                shell.store(pid, Ordering::Release);
            }
            // The one blocking `wait` in the session, and the only dependable
            // signal that the shell has exited — ConPTY will not report it on
            // the output pipe. It reaps the child as a side effect.
            let _ = child.wait();
            shell.store(0, Ordering::Release);
            drop(child);
            let _ = replies.send(Command::ChildExited);
        });
    let waiter = match waiter {
        Ok(waiter) => waiter,
        Err(error) => {
            let _ = killer.kill();
            drop(master);
            sink.close(PtyEvent::Error(format!(
                "could not start the pty waiter thread: {error}"
            )));
            return;
        }
    };

    let mut reader_done = false;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Input(data) => {
                if let Err(error) = writer.write_all(&data) {
                    log::warn!("local pty write failed: {error}");
                    break;
                }
                // The input pipe is buffered, and a keystroke left sitting in
                // it is a keystroke the shell never sees.
                if let Err(error) = writer.flush() {
                    log::warn!("local pty flush failed: {error}");
                    break;
                }
            }
            Command::Resize(cols, rows) => {
                if let Err(error) = master.resize(pty_size(cols, rows)) {
                    // Not fatal: the shell keeps running at the old size, and
                    // the next resize may well succeed.
                    log::warn!("local pty resize failed: {error}");
                }
            }
            Command::ReaderDone => {
                reader_done = true;
                break;
            }
            Command::Shutdown | Command::ChildExited => break,
        }
    }

    // A kill that fails means the child is already gone, which is the outcome
    // this wanted anyway.
    let _ = killer.kill();
    // Nothing may be written now, and closing the input side is part of
    // hanging up on the child.
    drop(writer);
    // The waiter owns the child, so joining it is how the child is reaped: by
    // the time this returns the process really has gone.
    let _ = waiter.join();
    // Closing the pseudoconsole is what breaks the output pipe, and so what
    // wakes a reader still parked in a `read`. Bytes already in the pipe are
    // delivered before the break, so nothing the child wrote is lost.
    drop(master);

    if !reader_done {
        wait_for_reader(&commands);
    }

    sink.close(PtyEvent::Exited);
}

/// Waits, briefly, for the reader thread to report that the pipe is drained.
///
/// The reader publishes straight to the sink, so `Exited` must not be closed
/// out from under it while output from the child's last moments is still in
/// flight. Other commands may arrive meanwhile — a handle being dropped, say —
/// and are discarded: the session is already ending.
fn wait_for_reader(commands: &Receiver<Command>) {
    let deadline = Instant::now() + READER_DRAIN_GRACE;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            log::warn!("local pty reader did not finish; ending the session anyway");
            return;
        };
        match commands.recv_timeout(remaining) {
            Ok(Command::ReaderDone) => return,
            Ok(_) => continue,
            // Both the reader and the handle are gone; there is nothing left
            // that could still publish.
            Err(_) => return,
        }
    }
}

/// The reader thread: blocking reads until the output pipe reports end of
/// stream.
///
/// It owns a share of the sink rather than borrowing one: it can outlive the
/// control thread, parked in a `read` that a background process keeps alive,
/// long after the session has been closed out.
fn read_loop(mut reader: Box<dyn Read + Send>, sink: Arc<EventSink>, replies: &Sender<Command>) {
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if !sink.emit(PtyEvent::Data(buffer[..count].to_vec())) {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                // A closed pseudoconsole is reported as a broken — or never
                // reconnected — pipe rather than as end of file, so that is the
                // ordinary way a session ends here, not a failure worth
                // logging.
                if !matches!(
                    error.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::NotConnected
                ) {
                    log::warn!("local pty read failed: {error}");
                }
                break;
            }
        }
    }
    let _ = replies.send(Command::ReaderDone);
}

/// Whether `parent` has a child process, read from a snapshot of every
/// process on the machine; `None` when no snapshot could be taken.
///
/// Windows records the ID of the process that created each process, and never
/// updates it: once that creator has exited, its ID may be handed to a new
/// process, which then appears to have children it never started. Those are
/// told apart by age — a child cannot be older than its parent. A process that
/// cannot be asked its age, or that has exited since the snapshot, is counted
/// as a child all the same, which at worst asks before a close that did not
/// need to.
fn has_child(parent: u32) -> Option<bool> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let parent_started = started_at(parent)?;
    // SAFETY: a plain call; the handle it returns is owned by `Handle` below.
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `entry` is a valid `PROCESSENTRY32W` with `dwSize` set, as both
    // calls require, and the snapshot handle is open for as long as `snapshot`
    // lives.
    let mut more = unsafe { Process32FirstW(snapshot.0, &raw mut entry) }.is_ok();
    while more {
        if entry.th32ParentProcessID == parent
            && entry.th32ProcessID != parent
            && started_at(entry.th32ProcessID).is_none_or(|started| started >= parent_started)
        {
            return Some(true);
        }
        // SAFETY: as for `Process32FirstW` above.
        more = unsafe { Process32NextW(snapshot.0, &raw mut entry) }.is_ok();
    }
    Some(false)
}

/// When process `pid` was created, in the 100-nanosecond ticks of a
/// `FILETIME`; `None` when it cannot be opened, which includes having exited.
fn started_at(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: a plain call; the handle it returns is owned by `Handle` below.
    let process =
        Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the handle is open with the access `GetProcessTimes` needs, and
    // every pointer is to a live `FILETIME` on this stack frame.
    unsafe {
        GetProcessTimes(
            process.0,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .ok()?;
    Some(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

/// A Win32 handle closed when this goes out of scope.
struct Handle(windows::Win32::Foundation::HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful call and is closed once,
        // here. A failure to close leaves nothing for this side to do.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Describes the child process: the configured command, or a plain shell.
fn command_builder(config: &PtyConfig) -> CommandBuilder {
    // An empty command list is treated as "no command": it carries no program
    // to run, and passing it on would only produce a spawn failure.
    let program = config
        .command
        .as_ref()
        .and_then(|command| command.split_first());

    let mut builder = match program {
        Some((program, args)) => {
            let mut builder = CommandBuilder::new(program);
            builder.args(args);
            builder
        }
        None => CommandBuilder::new(DEFAULT_SHELL),
    };

    // Only the two variables that describe *this* terminal, and whatever the
    // configuration adds, are set here; the rest of the child's environment is
    // inherited, which is what a user launching a shell expects. `TERM` means
    // little to a native Windows program, but a great deal to the ported unix
    // tools people run in one.
    builder.env("TERM", &config.term);
    builder.env("COLORTERM", "truecolor");
    for (key, value) in &config.env {
        builder.env(key, value);
    }

    // Left unset the child starts in the user's profile directory, which is
    // `CommandBuilder`'s own default and a better answer for a shell than
    // whatever directory this process happens to be running from.
    if let Some(cwd) = config.cwd.as_ref() {
        builder.cwd(cwd);
    }

    builder
}

/// Builds the size for a pty of `cols` by `rows`.
///
/// The pixel dimensions stay zero: nothing in a local shell reads them, and an
/// invented value is worse than none for the programs that do.
fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        // A zero-sized pty makes full-screen programs behave bizarrely, and
        // the UI may well ask for one before the first layout has happened.
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}
