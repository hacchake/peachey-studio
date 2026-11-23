//! Opening MIDI input ports and keeping them open as devices come and go.
//!
//! Talking to the system's MIDI service can block: the first Core MIDI
//! client of a process takes about three quarters of a second to create, and
//! closing a Windows device waits for its callback to finish. So every call
//! into `midir` that is not a callback happens on one worker thread owned by
//! [`MidiInput`]. The methods the UI calls only queue a request or read the
//! worker's last result, without waiting for the MIDI service.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::message::{MidiEvent, Parser};

/// Name of the MIDI client Peachey Studio registers with the system.
const CLIENT_NAME: &str = "Peachey Studio";
/// Longest wait before trying a port that failed to open again.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Receives every parsed message, on the thread of the port it came from.
/// Several ports means several threads, so the handler must be thread-safe,
/// and it runs inside the system's MIDI callback: do not block or allocate
/// there.
pub type MidiHandler = dyn Fn(&MidiEvent) + Send + Sync + 'static;

/// A MIDI input port the system offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiPortInfo {
    /// Opaque identifier, stable while the device stays plugged in. Two
    /// identical keyboards have the same name and different ids.
    pub id: String,
    /// Display name. WinMM (Windows) truncates it to 31 characters and
    /// often shows a generic driver name such as "USB Audio Device".
    pub name: String,
}

/// Which ports [`MidiInput`] keeps open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PortSelection {
    /// Every input port, including ones plugged in later.
    #[default]
    All,
    /// Only the port with this [`MidiPortInfo::id`]; reconnects if it
    /// disappears and comes back.
    Only(String),
    /// No ports.
    Nothing,
}

/// Lists the MIDI input ports without opening any, on the calling thread.
/// Blocks for the first Core MIDI client of the process (about 0.7 s), so
/// call it from a tool or a worker; an application should use
/// [`MidiInput::ports`]. Returns an empty list when the system has no MIDI
/// support available (for example no ALSA sequencer in a container).
pub fn list_inputs() -> Vec<MidiPortInfo> {
    match midir::MidiInput::new(CLIENT_NAME) {
        Ok(client) => scan(&client).into_iter().map(|(info, _)| info).collect(),
        Err(e) => {
            log::debug!("MIDI input unavailable: {e}");
            Vec::new()
        }
    }
}

fn scan(client: &midir::MidiInput) -> Vec<(MidiPortInfo, midir::MidiInputPort)> {
    client
        .ports()
        .into_iter()
        .filter_map(|port| {
            // A port that vanishes between listing and naming is skipped.
            let name = client.port_name(&port).ok()?;
            Some((
                MidiPortInfo {
                    id: port.id(),
                    name,
                },
                port,
            ))
        })
        .collect()
}

// ----------------------------------------------------------------------
// Public handle
// ----------------------------------------------------------------------

#[derive(Default)]
struct Snapshot {
    available: Vec<MidiPortInfo>,
    connected: Vec<MidiPortInfo>,
    selection: PortSelection,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    /// The set of open ports changed since `poll_ports` last said so.
    changed: AtomicBool,
    events: AtomicU64,
    callbacks: RwLock<u64>,
}

impl Shared {
    fn snapshot(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn deliver(&self, generation: u64, event: &MidiEvent, handler: &MidiHandler) {
        let current = self
            .callbacks
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if *current == generation {
            self.events.fetch_add(1, Ordering::Relaxed);
            handler(event);
        }
    }
}

enum Command {
    Poll,
    Select(PortSelection, u64),
    CloseAll,
    Flush(Sender<()>),
    Quit,
}

/// Opens MIDI input ports and forwards what they receive to a handler.
///
/// [`MidiInput::new`] starts a worker thread that opens every port. Call
/// [`poll_ports`](MidiInput::poll_ports) every couple of seconds to pick up
/// keyboards plugged in later and let go of unplugged ones: it only queues a
/// rescan, so it is safe on the UI thread. Messages are parsed on each port's
/// own callback thread and passed to the handler there, so a key press never
/// waits for the UI. Dropping the `MidiInput` closes every port.
pub struct MidiInput {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl MidiInput {
    /// Opens every input port and sends each message to `handler`.
    pub fn new(handler: impl Fn(&MidiEvent) + Send + Sync + 'static) -> MidiInput {
        MidiInput::with_selection(handler, PortSelection::All)
    }

    pub fn with_selection(
        handler: impl Fn(&MidiEvent) + Send + Sync + 'static,
        selection: PortSelection,
    ) -> MidiInput {
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(Snapshot {
                selection: selection.clone(),
                ..Snapshot::default()
            }),
            changed: AtomicBool::new(false),
            events: AtomicU64::new(0),
            callbacks: RwLock::new(0),
        });
        let (commands, rx) = channel();
        let worker_shared = shared.clone();
        let handler: Arc<MidiHandler> = Arc::new(handler);
        let thread = std::thread::Builder::new()
            .name("peel-midi-ports".into())
            .spawn(move || run(rx, worker_shared, handler, selection))
            .map_err(|e| log::warn!("could not start the MIDI port thread: {e}"))
            .ok();
        let input = MidiInput {
            shared,
            commands,
            thread,
        };
        input.poll_ports();
        input
    }

    /// Asks the worker to rescan the ports: it closes connections whose
    /// device is gone (or no longer selected) and opens new ones. Never
    /// blocks. Returns true if the set of open ports changed since the last
    /// call, so the UI can refresh a device list.
    pub fn poll_ports(&self) -> bool {
        let _ = self.commands.send(Command::Poll);
        self.shared.changed.swap(false, Ordering::Relaxed)
    }

    /// Chooses which ports to keep open. Takes effect on the worker thread.
    pub fn select(&self, selection: PortSelection) {
        // Wait only for an in-flight user handler, never for port operations.
        // Once this returns no callback belonging to the old selection can
        // arrive after the session flushes its live notes and expression.
        let generation = {
            let mut current = self
                .shared
                .callbacks
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            *current = current.wrapping_add(1);
            *current
        };
        self.shared.snapshot().selection = selection.clone();
        let _ = self.commands.send(Command::Select(selection, generation));
    }

    pub fn selection(&self) -> PortSelection {
        self.shared.snapshot().selection.clone()
    }

    /// Every input port seen by the last scan.
    pub fn ports(&self) -> Vec<MidiPortInfo> {
        self.shared.snapshot().available.clone()
    }

    /// The ports that are open right now.
    pub fn connected(&self) -> Vec<MidiPortInfo> {
        self.shared.snapshot().connected.clone()
    }

    /// Messages received on all ports so far (for an activity light).
    pub fn event_count(&self) -> u64 {
        self.shared.events.load(Ordering::Relaxed)
    }

    /// Closes every port. They reopen on the next
    /// [`poll_ports`](MidiInput::poll_ports) unless the selection is
    /// [`PortSelection::Nothing`].
    pub fn close_all(&self) {
        let _ = self.commands.send(Command::CloseAll);
    }

    /// Blocks until the worker has finished everything requested so far.
    /// For tests and command-line tools; the UI must not call it, because the
    /// first scan can take a second.
    pub fn flush(&self) {
        let (tx, rx) = channel();
        if self.commands.send(Command::Flush(tx)).is_ok() {
            let _ = rx.recv();
        }
    }
}

impl Drop for MidiInput {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ----------------------------------------------------------------------
// Worker thread
// ----------------------------------------------------------------------

fn run(
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    handler: Arc<MidiHandler>,
    selection: PortSelection,
) {
    let mut worker = Worker {
        shared,
        handler,
        scanner: None,
        selection,
        generation: 0,
        open: Vec::new(),
        failures: HashMap::new(),
    };
    while let Ok(first) = rx.recv() {
        // Everything queued so far is handled together, and any number of
        // polls collapse into one scan.
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }
        let mut rescan = false;
        let mut quit = false;
        for command in batch {
            match command {
                Command::Poll => rescan = true,
                Command::Select(selection, generation) => {
                    // Even retained ports need callbacks from the new generation.
                    worker.close_all();
                    worker.selection = selection;
                    worker.generation = generation;
                    rescan = true;
                }
                Command::CloseAll => {
                    worker.close_all();
                    rescan = false;
                }
                Command::Flush(done) => {
                    if rescan {
                        worker.poll();
                        rescan = false;
                    }
                    let _ = done.send(());
                }
                Command::Quit => quit = true,
            }
        }
        if rescan && !quit {
            worker.poll();
        }
        if quit {
            break;
        }
    }
    worker.close_all();
}

struct Connection {
    info: MidiPortInfo,
    conn: midir::MidiInputConnection<Parser>,
}

struct Failure {
    attempts: u32,
    retry_at: Instant,
}

struct Worker {
    shared: Arc<Shared>,
    handler: Arc<MidiHandler>,
    /// Used only to list ports; every connection gets a client of its own.
    scanner: Option<midir::MidiInput>,
    selection: PortSelection,
    generation: u64,
    open: Vec<Connection>,
    failures: HashMap<String, Failure>,
}

impl Worker {
    fn poll(&mut self) {
        if self.scanner.is_none() {
            self.scanner = match midir::MidiInput::new(CLIENT_NAME) {
                Ok(client) => Some(client),
                Err(e) => {
                    log::debug!("MIDI input unavailable: {e}");
                    None
                }
            };
        }
        let found = self.scanner.as_ref().map(scan).unwrap_or_default();
        let available: Vec<MidiPortInfo> = found.iter().map(|(info, _)| info.clone()).collect();
        let wanted: Vec<&(MidiPortInfo, midir::MidiInputPort)> = found
            .iter()
            .filter(|(info, _)| match &self.selection {
                PortSelection::All => true,
                PortSelection::Only(id) => *id == info.id,
                PortSelection::Nothing => false,
            })
            .collect();

        let mut changed = false;
        for c in std::mem::take(&mut self.open) {
            if wanted.iter().any(|(info, _)| info.id == c.info.id) {
                self.open.push(c);
            } else {
                log::info!("MIDI input closed: {}", c.info.name);
                c.conn.close();
                changed = true;
            }
        }
        // A port that vanished gets a fresh start when it returns.
        self.failures
            .retain(|id, _| available.iter().any(|a| a.id == *id));

        for (info, port) in wanted {
            if self.open.iter().any(|c| c.info.id == info.id) {
                continue;
            }
            if self
                .failures
                .get(&info.id)
                .is_some_and(|f| Instant::now() < f.retry_at)
            {
                continue;
            }
            match self.connect(port) {
                Ok(conn) => {
                    log::info!("MIDI input opened: {}", info.name);
                    self.failures.remove(&info.id);
                    self.open.push(Connection {
                        info: info.clone(),
                        conn,
                    });
                    changed = true;
                }
                Err(e) => {
                    let attempts = self.failures.get(&info.id).map_or(0, |f| f.attempts) + 1;
                    let wait = Duration::from_secs(2u64 << attempts.min(5)).min(MAX_RETRY_WAIT);
                    log::warn!("could not open MIDI input {}: {e}", info.name);
                    self.failures.insert(
                        info.id.clone(),
                        Failure {
                            attempts,
                            retry_at: Instant::now() + wait,
                        },
                    );
                }
            }
        }
        self.publish(available, changed);
    }

    fn connect(
        &self,
        port: &midir::MidiInputPort,
    ) -> Result<midir::MidiInputConnection<Parser>, String> {
        // `connect` consumes its client, so each port gets its own.
        let mut client = midir::MidiInput::new(CLIENT_NAME).map_err(|e| e.to_string())?;
        // System exclusive, MIDI clock and active sensing are not used, and
        // the clock alone is 24 callbacks per beat.
        client.ignore(midir::Ignore::All);
        let handler = self.handler.clone();
        let shared = self.shared.clone();
        let generation = self.generation;
        client
            .connect(
                port,
                &format!("{CLIENT_NAME} input"),
                move |_stamp, bytes, parser| {
                    parser.parse(bytes, |event| {
                        shared.deliver(generation, &event, handler.as_ref());
                    });
                },
                Parser::new(),
            )
            .map_err(|e| e.to_string())
    }

    fn close_all(&mut self) {
        let any = !self.open.is_empty();
        for c in std::mem::take(&mut self.open) {
            c.conn.close();
        }
        self.failures.clear();
        if any {
            let available = self.shared.snapshot().available.clone();
            self.publish(available, true);
        }
    }

    fn publish(&self, available: Vec<MidiPortInfo>, changed: bool) {
        let connected = self.open.iter().map(|c| c.info.clone()).collect();
        let mut snapshot = self.shared.snapshot();
        snapshot.available = available;
        snapshot.connected = connected;
        drop(snapshot);
        if changed {
            self.shared.changed.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod callback_tests {
    use super::*;

    #[test]
    fn old_port_callbacks_cannot_arrive_after_selection_reset() {
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(Snapshot::default()),
            changed: AtomicBool::new(false),
            events: AtomicU64::new(0),
            callbacks: RwLock::new(0),
        });
        let seen = Arc::new(AtomicU64::new(0));
        let count = seen.clone();
        let handler: Arc<MidiHandler> = Arc::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
        });
        let event = MidiEvent::PitchBend {
            channel: 0,
            value: 8191,
        };
        shared.deliver(0, &event, handler.as_ref());
        let (commands, requests) = channel();
        let input = MidiInput {
            shared: shared.clone(),
            commands,
            thread: None,
        };
        input.select(PortSelection::Nothing);
        let Command::Select(PortSelection::Nothing, generation) = requests.recv().unwrap() else {
            panic!("selection request");
        };
        // The session clears expression only after select invalidates old callbacks.
        seen.store(0, Ordering::Relaxed);
        shared.deliver(0, &event, handler.as_ref());
        assert_eq!(
            seen.load(Ordering::Relaxed),
            0,
            "late old-port expression was rejected"
        );
        shared.deliver(generation, &event, handler.as_ref());
        assert_eq!(
            seen.load(Ordering::Relaxed),
            1,
            "new-port expression still arrives"
        );
    }
}
