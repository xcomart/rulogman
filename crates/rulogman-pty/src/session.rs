//! The local pty session driver for unix.
//!
//! [`PtySession::spawn`] moves every blocking operation onto OS threads this
//! crate owns, so a GUI thread can hold a [`PtySession`] and never block on
//! it. All communication happens through channels: commands flow in,
//! [`PtyEvent`]s flow out.
//!
//! Three threads do the work. A *control* thread owns the pty: it opens it,
//! applies resizes, and — last of all — drops it, which hangs up on the shell
//! and reaps it. A *reader* thread does nothing but blocking reads from a
//! duplicate of the master, and a *writer* thread nothing but blocking writes
//! of keystrokes to another. Splitting them is what lets a write happen while
//! a read is parked, without a poller and without either side spinning — and
//! what keeps a write that cannot finish, into a program that has stopped
//! reading its input, from holding up a resize or the hang-up that closes the
//! tab.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, Shell};

use crate::config::PtyConfig;
use crate::driver::{Command, EventSink, READ_BUFFER};
use crate::event::PtyEvent;

/// A live (or recently ended) local shell running on its own threads.
///
/// The handle is `Send` and `Sync` and every method is non-blocking, so it can
/// be held and used from a GUI thread. Dropping it ends the session.
pub struct PtySession {
    /// Command channel to the control thread.
    commands: Sender<Command>,
    /// Process ID of the program started on the pty, or `0` while there is
    /// none — before it has been started and once it has been reaped.
    ///
    /// Written by the control thread, which is the one that owns the child,
    /// and read by [`PtySession::current_dir`].
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
    /// Spawns the user's login shell on a new pty. Never blocks the caller.
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
            .spawn(move || control(config, worker_sink, command_rx, command_tx, &shell));

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
    /// Reaches the child as `SIGWINCH`. Silently ignored after the session has
    /// ended.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.commands.send(Command::Resize(cols, rows));
    }

    /// Ends the session. Safe to call more than once.
    ///
    /// The shell is hung up on with `SIGHUP` and reaped, exactly as a closing
    /// terminal emulator would do it.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }

    /// The directory the shell — or the program in the foreground of its pty
    /// — is working in, as the operating system reports it.
    ///
    /// For a shell that does not announce its directory over `OSC 7`, which is
    /// most of them out of the box. `None` before the shell has started, once
    /// it has exited, and on a platform with no way to ask; see
    /// [`crate::process`]. Asks the kernel directly, which is a `readlink` or
    /// two on Linux and a pair of `proc_pidinfo` calls on macOS: cheap enough
    /// for a click, not for a frame.
    pub fn current_dir(&self) -> Option<PathBuf> {
        match self.shell.load(Ordering::Acquire) {
            0 => None,
            pid => crate::process::current_dir(pid),
        }
    }
}

/// Publishes the child's process ID for as long as this lives, and withdraws it
/// on the way out — before the child is reaped, so that a reader never sees an
/// ID the kernel may already have handed to somebody else.
struct Published<'a>(&'a AtomicU32);

impl<'a> Published<'a> {
    fn new(slot: &'a AtomicU32, pid: u32) -> Self {
        slot.store(pid, Ordering::Release);
        Self(slot)
    }
}

impl Drop for Published<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

impl Drop for PtySession {
    /// A dropped handle must not leave a shell — and its children — running
    /// invisibly for the rest of the process's life.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The control thread: owns the pty for as long as the session lives.
fn control(
    config: PtyConfig,
    sink: Arc<EventSink>,
    commands: Receiver<Command>,
    replies: Sender<Command>,
    shell: &AtomicU32,
) {
    let mut pty = match open_pty(&config) {
        Ok(pty) => pty,
        Err(error) => {
            sink.close(PtyEvent::Error(format!(
                "could not start the local shell: {error}"
            )));
            return;
        }
    };
    // Declared after the pty, so that every early return below drops it first
    // and the ID is gone before the child is.
    let published = Published::new(shell, pty.child().id());

    // `tty::new` leaves the master non-blocking for alacritty's own poller.
    // We read on a dedicated thread instead, where a blocking read is both
    // simpler and cheaper than spinning on `WouldBlock`.
    if let Err(error) = set_blocking(pty.file()) {
        sink.close(PtyEvent::Error(format!(
            "could not configure the local pty: {error}"
        )));
        return;
    }

    // The reader and the writer get descriptors of their own so that reads,
    // writes and this thread are independent; all refer to the same master.
    let (reader, writer) = match pty
        .file()
        .try_clone()
        .and_then(|reader| Ok((reader, pty.file().try_clone()?)))
    {
        Ok(pair) => pair,
        Err(error) => {
            sink.close(PtyEvent::Error(format!(
                "could not duplicate the local pty: {error}"
            )));
            return;
        }
    };

    sink.emit(PtyEvent::Ready);

    let reader_sink = Arc::clone(&sink);
    let spawned = std::thread::Builder::new()
        .name("rulogman-pty-reader".to_owned())
        .spawn(move || read_loop(reader, reader_sink, &replies));
    if let Err(error) = spawned {
        sink.close(PtyEvent::Error(format!(
            "could not start the pty reader thread: {error}"
        )));
        return;
    }

    let (input, input_queue) = channel::<Vec<u8>>();
    let spawned = std::thread::Builder::new()
        .name("rulogman-pty-writer".to_owned())
        .spawn(move || write_loop(writer, &input_queue));
    if let Err(error) = spawned {
        sink.close(PtyEvent::Error(format!(
            "could not start the pty writer thread: {error}"
        )));
        return;
    }

    // `replies` now lives only in the reader thread, so a disconnected channel
    // means both the handle and the reader are gone — shut down either way.
    while let Ok(command) = commands.recv() {
        match command {
            // Handed over rather than written here. A failed hand-over means
            // the writer has stopped, which it only does when a write failed.
            Command::Input(data) => {
                if input.send(data).is_err() {
                    break;
                }
            }
            Command::Resize(cols, rows) => pty.on_resize(window_size(cols, rows)),
            Command::Shutdown | Command::ReaderDone => break,
        }
    }

    // Dropping the pty sends `SIGHUP` to the shell and waits for it, so by the
    // time `Exited` is published the child really is gone.
    drop(published);
    drop(pty);
    sink.close(PtyEvent::Exited);
}

/// The reader thread: blocking reads until the master reports end of stream.
///
/// It owns a share of the sink rather than borrowing one: it can outlive the
/// control thread, parked in a `read` that a background process keeps alive,
/// long after the session has been closed out.
fn read_loop(mut reader: File, sink: Arc<EventSink>, replies: &Sender<Command>) {
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if !sink.emit(PtyEvent::Data(buffer[..count].to_vec())) {
                    break;
                }
            }
            // A signal interrupted the read; nothing was lost.
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                // Linux reports the closing of the last slave as `EIO` rather
                // than as end of file, so that is the normal way a shell exit
                // is observed here, not a failure worth logging.
                if error.raw_os_error() != Some(libc::EIO) {
                    log::warn!("local pty read failed: {error}");
                }
                break;
            }
        }
    }
    let _ = replies.send(Command::ReaderDone);
}

/// The writer thread: writes queued input to the master, in order, until a
/// write fails or the control thread lets go of the queue.
///
/// Like the reader, it can outlive the control thread: parked in a `write` to a
/// program that never reads, it is released only when the last process holding
/// the pty's other end lets go of it.
fn write_loop(mut writer: File, input: &Receiver<Vec<u8>>) {
    while let Ok(data) = input.recv() {
        if let Err(error) = writer.write_all(&data) {
            // `EIO` is the shell having gone, as it is for the reader.
            if error.raw_os_error() != Some(libc::EIO) {
                log::warn!("local pty write failed: {error}");
            }
            break;
        }
    }
}

/// Opens a pty and starts the configured program — or the login shell — on it.
fn open_pty(config: &PtyConfig) -> io::Result<tty::Pty> {
    // Only the two variables that describe *this* terminal, and whatever the
    // configuration adds, are set here; the rest of the child's environment is
    // inherited, which is what a user launching a shell expects.
    let mut env = HashMap::with_capacity(2 + config.env.len());
    env.insert("TERM".to_owned(), config.term.clone());
    env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
    env.extend(config.env.iter().cloned());

    // An empty command list is treated as "no command": it carries no program
    // to run, and passing it on would only produce a spawn failure.
    let shell = config.command.as_ref().and_then(|command| {
        let (program, args) = command.split_first()?;
        Some(Shell::new(program.clone(), args.to_vec()))
    });

    let options = tty::Options {
        shell,
        working_directory: config.cwd.clone(),
        // Only consulted by alacritty's own event loop, which we do not use:
        // the shell's output is published as it arrives, up to the end of the
        // stream, and `Exited` follows it.
        drain_on_exit: false,
        env,
    };

    tty::new(&options, window_size(config.cols, config.rows), 0)
}

/// Builds the window size for a pty of `cols` by `rows`.
///
/// The pixel dimensions stay zero: nothing in a local shell reads them, and an
/// invented value is worse than none for the programs that do.
fn window_size(cols: u16, rows: u16) -> WindowSize {
    WindowSize {
        // A zero-sized pty makes full-screen programs behave bizarrely, and
        // the UI may well ask for one before the first layout has happened.
        num_lines: rows.max(1),
        num_cols: cols.max(1),
        cell_width: 0,
        cell_height: 0,
    }
}

/// Clears `O_NONBLOCK` on the pty master.
fn set_blocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is owned by the caller's still-live `File`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; the flags are the ones just read back.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
