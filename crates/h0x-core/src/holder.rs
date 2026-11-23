//! The holder: owns every PTY, is every program's parent, always reads output.
//! Spawn, pump, resize, signal, reap. No parsing, no policy. It keeps a ~1 MB
//! tail per terminal so a core that crashed can rebuild screens, and restarts
//! a crashed core itself.

use crate::holder_proto::{self as proto, Exit, Goodbye, HeldTerminal};
use crate::paths::Paths;
use crate::platform::{self, Process, Pty, Stream};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 1 << 20;
const EXIT_DRAIN: Duration = Duration::from_millis(200);
/// Output queued for a core that reads slowly before programs have to wait for it.
const BACKLOG_BYTES: usize = 8 << 20;

struct Term {
    pid: u32,
    pty: Arc<Pty>,
    /// Input goes through its own thread: a program that stops reading blocks only itself.
    input: std::sync::mpsc::Sender<Vec<u8>>,
    cols: u16,
    rows: u16,
    tail: VecDeque<u8>,
    exit: Option<Exit>,
    reader_done: bool,
    /// Since when its output could drain; None while its reader waits for a
    /// core that fell behind. An exit waits for its output from then on.
    drains_since: Option<Instant>,
    /// Its `H0X_SCOPE` token: what it started carries it too.
    scope: Option<String>,
}

/// The connected core. Frames go out through its own writer thread: a core
/// that stops reading must never block anyone holding the state lock.
struct Core {
    generation: u64,
    frames: std::sync::mpsc::Sender<Vec<u8>>,
    backlog: Arc<Backlog>,
    /// Shut down to free a writer stuck on a core that is gone.
    stream: Stream,
}

/// Bytes queued for one core; None once its writer stopped.
struct Backlog {
    bytes: Mutex<Option<usize>>,
    changed: Condvar,
}

impl Backlog {
    fn update(&self, f: impl FnOnce(&mut Option<usize>)) {
        f(&mut self.bytes.lock().unwrap_or_else(|p| p.into_inner()));
        self.changed.notify_all();
    }

    fn over(&self, limit: usize) -> bool {
        self.bytes.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|n| n > limit)
    }

    /// Waits (never under the state lock) until at most `limit` bytes are queued or the writer stopped.
    fn wait(&self, limit: usize, timeout: Duration) {
        let bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self.changed.wait_timeout_while(bytes, timeout, |b| b.is_some_and(|n| n > limit));
    }
}

struct State {
    terms: HashMap<u32, Term>,
    core: Option<Core>,
    generation: u64,
    previous: &'static str,
    core_left: Instant,
    restarts: Vec<Instant>,
}

struct Holder {
    state: Mutex<State>,
    changed: Condvar,
    instance: String,
    grace: Duration,
    paths: Paths,
    socket: std::path::PathBuf,
    /// A newer build took over: exit once the last terminal is gone.
    draining: std::sync::atomic::AtomicBool,
    /// Sweeps still ending what a finished program left behind.
    sweeps: std::sync::atomic::AtomicUsize,
}

/// Runs a holder on `socket` (each holder has its own; the core picks the path).
pub fn run(socket: std::path::PathBuf) -> io::Result<()> {
    platform::raise_nofile_limit();
    let paths = Paths::from_env().map_err(|e| io::Error::other(e.message))?;
    paths.create_dirs()?;
    // A taken path means another holder lives there; a stale file is removed by the core.
    let listener = platform::listen(&socket)?;
    let grace = std::env::var("H0X_CORE_HOLDER_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(3), Duration::from_millis);
    let holder = Arc::new(Holder {
        state: Mutex::new(State {
            terms: HashMap::new(),
            core: None,
            generation: 0,
            previous: "none",
            core_left: Instant::now(),
            restarts: Vec::new(),
        }),
        changed: Condvar::new(),
        instance: crate::platform::random_hex(),
        grace,
        paths,
        socket,
        draining: Default::default(),
        sweeps: Default::default(),
    });
    let supervisor = holder.clone();
    std::thread::spawn(move || supervisor.supervise());
    for stream in listener.incoming() {
        let holder = holder.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(e) = holder.serve(stream) {
                let _ = writeln!(io::stderr(), "holder: core connection: {e}");
            }
        });
    }
    Ok(())
}

impl Holder {
    fn exit(&self) -> ! {
        platform::remove_endpoint(&self.socket);
        // Never leave with leftovers still being ended: nobody else would.
        self.wait_for_sweeps();
        std::process::exit(0);
    }

    fn wait_for_sweeps(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.sweeps.load(std::sync::atomic::Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Ends everything carrying `scope`, in the background.
    fn sweep(self: &Arc<Self>, scope: Option<String>) {
        let Some(scope) = scope else { return };
        self.sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let holder = self.clone();
        std::thread::spawn(move || {
            crate::sweep::sweep(&[scope]);
            holder.sweeps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }

    fn exit_if_drained(&self) {
        if self.draining.load(std::sync::atomic::Ordering::SeqCst) && self.lock().terms.is_empty() {
            self.exit();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queues a frame for the current core.
    fn to_core(&self, st: &mut State, frame: Vec<u8>) {
        if let Some(core) = &st.core {
            core.backlog.update(|b| *b = b.map(|n| n + frame.len()));
            let _ = core.frames.send(frame);
        }
    }

    /// A handover lets the writer deliver what is queued (BYE last) but no longer
    /// holds programs back for it; any other end cuts it off.
    fn core_gone(&self, st: &mut State, how: &'static str) {
        if let Some(core) = st.core.take() {
            match how {
                "handover" => core.backlog.update(|b| *b = None),
                _ => drop(core.stream.shutdown(std::net::Shutdown::Both)),
            }
        }
        st.previous = how;
        st.core_left = Instant::now();
    }

    /// Writes one core's frames in order; a failed write means that core is gone.
    fn write_to_core(&self, generation: u64, mut stream: Stream, frames: Receiver<Vec<u8>>, backlog: Arc<Backlog>) {
        const BATCH_BYTES: usize = 64 * 1024;
        let mut batch = Vec::new();
        let mut next = frames.recv().ok();
        while let Some(frame) = next.take() {
            // Frames of BATCH_BYTES or more are never copied; a lone frame is written as is.
            let second = if frame.len() >= BATCH_BYTES { None } else { frames.try_recv().ok() };
            let out: &[u8] = match second {
                None => &frame,
                Some(mut more) => {
                    batch.clear();
                    batch.extend_from_slice(&frame);
                    loop {
                        if more.len() >= BATCH_BYTES || batch.len() >= BATCH_BYTES {
                            next = Some(more);
                            break;
                        }
                        batch.extend_from_slice(&more);
                        match frames.try_recv() {
                            Ok(f) => more = f,
                            Err(_) => break,
                        }
                    }
                    &batch
                }
            };
            if stream.write_all(out).is_err() {
                break;
            }
            let written = out.len();
            backlog.update(|b| *b = b.map(|n| n.saturating_sub(written)));
            if next.is_none() {
                next = frames.recv().ok();
            }
        }
        backlog.update(|b| *b = None);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, "crash");
        }
    }

    fn serve(self: &Arc<Self>, mut stream: Stream) -> io::Result<()> {
        match proto::read(&mut stream)? {
            Some((proto::HELLO, _, payload)) => proto::parse::<proto::Hello>(&payload)?,
            _ => return Ok(()),
        };
        let generation = {
            let mut st = self.lock();
            if st.core.is_some() {
                self.core_gone(&mut st, "crash");
            }
            st.generation += 1;
            let mut ids: Vec<u32> = st.terms.keys().copied().collect();
            ids.sort();
            let welcome = proto::Welcome {
                version: proto::VERSION,
                instance: self.instance.clone(),
                pid: std::process::id(),
                build: crate::client::build_id(),
                previous: st.previous.into(),
                terminals: ids
                    .iter()
                    .map(|id| {
                        let t = &st.terms[id];
                        HeldTerminal { id: *id, pid: t.pid, cols: t.cols, rows: t.rows, exit: t.exit }
                    })
                    .collect(),
            };
            let mut out = proto::json_frame(proto::WELCOME, 0, &welcome);
            for id in &ids {
                let (a, b) = st.terms[id].tail.as_slices();
                out.extend(proto::frame(proto::TAIL, *id, &[a, b].concat()));
            }
            let (frames, queued) = std::sync::mpsc::channel();
            let backlog = Arc::new(Backlog { bytes: Mutex::new(Some(0)), changed: Condvar::new() });
            let (writer, kept) = (stream.try_clone()?, stream.try_clone()?);
            let (holder, written, generation) = (self.clone(), backlog.clone(), st.generation);
            std::thread::spawn(move || holder.write_to_core(generation, writer, queued, written));
            st.core = Some(Core { generation, frames, backlog, stream: kept });
            self.to_core(&mut st, out);
            generation
        };

        let result = self.commands(&mut stream, generation);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, "crash");
        }
        result
    }

    fn commands(self: &Arc<Self>, stream: &mut Stream, generation: u64) -> io::Result<()> {
        while let Some((kind, id, payload)) = proto::read(stream)? {
            match kind {
                proto::SPAWN => self.spawn(id, proto::parse(&payload)?),
                proto::INPUT => {
                    if let Some(t) = self.lock().terms.get(&id) {
                        let _ = t.input.send(payload);
                    }
                }
                proto::RESIZE => {
                    let size: proto::Size = proto::parse(&payload)?;
                    let mut st = self.lock();
                    if let Some(t) = st.terms.get_mut(&id) {
                        t.cols = size.cols;
                        t.rows = size.rows;
                        t.pty.resize(size.cols, size.rows);
                    }
                }
                proto::SIGNAL => {
                    let sig: proto::Signal = proto::parse(&payload)?;
                    let st = self.lock();
                    // Never signal after reaping: the pid may belong to someone else now.
                    if let Some(t) = st.terms.get(&id).filter(|t| t.exit.is_none()) {
                        t.pty.signal(t.pid, sig.signal);
                    }
                }
                proto::FOREGROUND => {
                    let mut st = self.lock();
                    let pgid = st.terms.get(&id).filter(|t| t.exit.is_none()).map_or(-1, |t| t.pty.foreground());
                    self.to_core(&mut st, proto::json_frame(proto::FOREGROUND_IS, id, &proto::Foreground { pgid }));
                }
                proto::RELEASE => {
                    if let Some(t) = self.lock().terms.remove(&id)
                        && !t.reader_done
                    {
                        // Leftover background jobs still hold the PTY: hang them up.
                        t.pty.hang_up(t.pid);
                    }
                    self.exit_if_drained();
                }
                proto::DRAIN => {
                    self.draining.store(true, std::sync::atomic::Ordering::SeqCst);
                    self.exit_if_drained();
                }
                proto::GOODBYE => {
                    let mode: Goodbye = proto::parse(&payload)?;
                    let mut st = self.lock();
                    // Read late, after a newer core said hello: that one stays.
                    if st.core.as_ref().is_none_or(|c| c.generation != generation) {
                        return Ok(());
                    }
                    if mode == Goodbye::Handover {
                        // The core saved screens up to here; the next one replays only what follows.
                        st.terms.values_mut().for_each(|t| t.tail.clear());
                    }
                    if mode == Goodbye::Shutdown {
                        // Whatever the core's stop missed (started since, or hiding) ends
                        // here, before the goodbye: the stop returns when all is gone.
                        let scopes: Vec<Option<String>> = st.terms.values().map(|t| t.scope.clone()).collect();
                        drop(st);
                        scopes.into_iter().for_each(|scope| self.sweep(scope));
                        self.wait_for_sweeps();
                        let mut st = self.lock();
                        self.to_core(&mut st, proto::frame(proto::BYE, 0, &[]));
                        let backlog = st.core.as_ref().map(|c| c.backlog.clone());
                        drop(st);
                        if let Some(backlog) = backlog {
                            backlog.wait(0, Duration::from_secs(5));
                        }
                        self.exit();
                    }
                    self.to_core(&mut st, proto::frame(proto::BYE, 0, &[]));
                    self.core_gone(&mut st, "handover");
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, id: u32, spec: proto::Spawn) {
        let mut st = self.lock();
        if let Some(t) = st.terms.get(&id) {
            let frame = proto::json_frame(proto::SPAWNED, id, &proto::Spawned { pid: t.pid });
            return self.to_core(&mut st, frame);
        }
        let started = platform::spawn_pty(&spec.argv, &spec.env, &spec.cwd, spec.cols, spec.rows)
            .and_then(|(pty, child)| Ok((pty.reader()?, pty.writer()?, pty, child)));
        let frame = match started {
            Err(e) => {
                let error = format!("cannot start {:?}: {e}", spec.argv.first().map_or("", |s| s.as_str()));
                let _ = writeln!(io::stderr(), "holder: {}: {error}", crate::terminals::address(id));
                proto::json_frame(proto::SPAWN_FAILED, id, &proto::SpawnFailed { error })
            }
            Ok((reader, mut writer, pty, child)) => {
                let pid = child.id();
                let (input, queued) = std::sync::mpsc::channel::<Vec<u8>>();
                std::thread::spawn(move || {
                    for bytes in queued {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                });
                st.terms.insert(
                    id,
                    Term {
                        pid,
                        input,
                        pty: Arc::new(pty),
                        cols: spec.cols,
                        rows: spec.rows,
                        tail: VecDeque::new(),
                        exit: None,
                        reader_done: false,
                        drains_since: Some(Instant::now()),
                        scope: spec.env.get(crate::sweep::VAR).cloned(),
                    },
                );
                let (holder, waiter) = (self.clone(), self.clone());
                let scope = spec.env.get(crate::sweep::VAR).cloned();
                std::thread::spawn(move || holder.pump(id, reader));
                std::thread::spawn(move || waiter.reap(id, child, scope));
                proto::json_frame(proto::SPAWNED, id, &proto::Spawned { pid })
            }
        };
        self.to_core(&mut st, frame);
    }

    /// Always reads, core or not, so programs never stall on a full PTY.
    fn pump(&self, id: u32, mut reader: impl Read) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // EIO: every slave side closed
            };
            let mut st = self.lock();
            let Some(t) = st.terms.get_mut(&id) else { break };
            t.tail.extend(&buf[..n]);
            let excess = t.tail.len().saturating_sub(TAIL_BYTES);
            t.tail.drain(..excess);
            self.to_core(&mut st, proto::frame(proto::OUTPUT, id, &buf[..n]));
            // A core that falls behind slows its programs, as a PTY nobody reads would.
            let Some(backlog) = st.core.as_ref().map(|c| c.backlog.clone()).filter(|b| b.over(BACKLOG_BYTES)) else {
                continue;
            };
            st.terms.get_mut(&id).expect("just found").drains_since = None;
            drop(st);
            backlog.wait(BACKLOG_BYTES, Duration::MAX);
            if let Some(t) = self.lock().terms.get_mut(&id) {
                t.drains_since = Some(Instant::now());
            }
            self.changed.notify_all();
        }
        if let Some(t) = self.lock().terms.get_mut(&id) {
            t.reader_done = true;
        }
        self.changed.notify_all();
    }

    /// The exit code comes from here, after the last output has been forwarded.
    /// `scope` is its own copy: the terminal may be released (forgotten) first.
    fn reap(self: &Arc<Self>, id: u32, mut child: Process, scope: Option<String>) {
        let (code, signal) = match platform::wait(&mut child) {
            Ok(ended) => ended,
            Err(e) => {
                let _ = writeln!(io::stderr(), "holder: wait {}: {e}", crate::terminals::address(id));
                return;
            }
        };
        let exit = Exit { code, signal };
        // The terminal lived exactly as long as its program: what it left behind
        // goes too, before the exit is reported. If we die first, the core still
        // sees it live and ends what carries its scope itself.
        if let Some(scope) = scope {
            self.sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::sweep::sweep(&[scope]);
            self.sweeps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let exited = Instant::now();
        let mut st = self.lock();
        while let Some(t) = st.terms.get(&id).filter(|t| !t.reader_done) {
            // Its output goes first: time a slow core holds the reader back doesn't count.
            let Some(since) = t.drains_since else {
                st = self.changed.wait(st).unwrap_or_else(|p| p.into_inner());
                continue;
            };
            let deadline = since.max(exited) + EXIT_DRAIN;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.changed.wait_timeout(st, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        if let Some(t) = st.terms.get_mut(&id) {
            t.exit = Some(exit);
            self.to_core(&mut st, proto::json_frame(proto::EXITED, id, &exit));
        }
    }

    /// Starts a core when the last one crashed and none came back in time.
    fn supervise(&self) {
        loop {
            std::thread::sleep(self.grace.min(Duration::from_millis(100)));
            let mut st = self.lock();
            if st.core.is_some() || st.core_left.elapsed() < self.grace {
                continue;
            }
            if st.terms.is_empty() {
                self.exit();
            }
            let now = Instant::now();
            st.restarts.retain(|t| now.duration_since(*t) < Duration::from_secs(300));
            let backoff = self.grace * 2u32.pow(st.restarts.len().min(6) as u32);
            if st.restarts.last().is_some_and(|t| now.duration_since(*t) < backoff.min(Duration::from_secs(60))) {
                continue;
            }
            st.restarts.push(now);
            st.core_left = now;
            drop(st);
            if let Err(e) = crate::client::spawn_core(&self.paths) {
                let _ = writeln!(io::stderr(), "holder: cannot start core: {e:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{HEADER, Outbox};
    use std::hint::black_box;

    const TOTAL: usize = 64 << 20;
    const RUNS: usize = 5;

    /// Prints the median of `RUNS` timings of `work` (TOTAL bytes in `chunk`-sized frames).
    fn report(name: &str, chunk: usize, mut work: impl FnMut()) {
        let mut runs: Vec<Duration> = (0..RUNS)
            .map(|_| {
                let at = Instant::now();
                work();
                at.elapsed()
            })
            .collect();
        runs.sort();
        let median = runs[RUNS / 2];
        let mb_s = TOTAL as f64 / 1e6 / median.as_secs_f64();
        let ns_frame = median.as_nanos() as f64 / (TOTAL / chunk) as f64;
        println!("perf {name:<9} {chunk:>5} B  {mb_s:>9.1} MB/s  {ns_frame:>8.1} ns/frame");
    }

    /// The byte path's hot spots: holder `to_core`, the viewer `Outbox`, and the
    /// core's frame reads from a holder link (unbuffered and buffered).
    #[test]
    #[ignore = "timing: cargo test --release -p h0x-core perf_byte_path -- --ignored --nocapture"]
    fn perf_byte_path_throughput() {
        #[cfg(windows)]
        let socket = std::path::PathBuf::from(format!(r"\\.\pipe\h0x-core-perf-{}", std::process::id()));
        #[cfg(unix)]
        let socket = std::env::temp_dir().join(format!("h0x-core-perf-{}.sock", std::process::id()));
        #[cfg(unix)]
        let _ = std::fs::remove_file(&socket);
        let listener = platform::listen(&socket).unwrap();
        let client = platform::connect(&socket).unwrap();
        let mut server = listener.incoming().next().unwrap().unwrap();

        let (frames, queued) = std::sync::mpsc::channel();
        let backlog = Arc::new(Backlog { bytes: Mutex::new(Some(0)), changed: Condvar::new() });
        let core = Core { generation: 1, frames, backlog, stream: client.try_clone().unwrap() };
        let holder = Holder {
            state: Mutex::new(State {
                terms: HashMap::new(),
                core: Some(core),
                generation: 1,
                previous: "",
                core_left: Instant::now(),
                restarts: Vec::new(),
            }),
            changed: Condvar::new(),
            instance: "perf".into(),
            grace: Duration::ZERO,
            paths: Paths {
                socket: socket.clone(),
                holder_socket: socket.clone(),
                state: socket.clone(),
                config: socket.clone(),
            },
            socket,
            draining: std::sync::atomic::AtomicBool::new(false),
            sweeps: std::sync::atomic::AtomicUsize::new(0),
        };

        for chunk in [4096, 256] {
            let data = vec![b'x'; chunk];
            let count = TOTAL / chunk;
            report("to_core", chunk, || {
                let mut st = holder.lock();
                for _ in 0..count {
                    holder.to_core(&mut st, proto::frame(proto::OUTPUT, 1, &data));
                    for frame in queued.try_iter() {
                        black_box(frame);
                    }
                }
            });
            report("outbox", chunk, || {
                let mut out = Outbox::default();
                out.open(1);
                for _ in 0..count {
                    out.data(1, &data);
                    while let Some(frame) = out.next_frame() {
                        out.credit(1, frame.len() - HEADER);
                        black_box(frame);
                    }
                }
            });
            let wire: Arc<Vec<u8>> = Arc::new((0..count).flat_map(|_| proto::frame(proto::OUTPUT, 1, &data)).collect());
            for (name, buffered) in [("read", false), ("read+buf", true)] {
                report(name, chunk, || {
                    let (wire, mut tx) = (wire.clone(), client.try_clone().unwrap());
                    let writer = std::thread::spawn(move || tx.write_all(&wire).unwrap());
                    if buffered {
                        let mut reader = io::BufReader::with_capacity(64 << 10, &mut server);
                        for _ in 0..count {
                            black_box(proto::read(&mut reader).unwrap().unwrap());
                        }
                    } else {
                        for _ in 0..count {
                            black_box(proto::read(&mut server).unwrap().unwrap());
                        }
                    }
                    writer.join().unwrap();
                });
            }
        }
    }

    const BATCH: usize = 64 * 1024;

    /// A connected (writer, reader) pair: a named pipe on Windows, a socket pair on Unix.
    fn linked() -> (Stream, Stream) {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::pair().unwrap()
        }
        #[cfg(windows)]
        {
            let socket = std::path::PathBuf::from(format!(r"\\.\pipe\h0x-core-test-{}", crate::platform::random_hex()));
            let listener = platform::listen(&socket).unwrap();
            let writer = platform::connect(&socket).unwrap();
            let reader = listener.incoming().next().unwrap().unwrap();
            (writer, reader)
        }
    }

    fn backlog(bytes: Option<usize>) -> Arc<Backlog> {
        Arc::new(Backlog { bytes: Mutex::new(bytes), changed: Condvar::new() })
    }

    fn queued(backlog: &Backlog) -> Option<usize> {
        *backlog.bytes.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn test_holder(generation: u64, core: Option<Core>) -> Arc<Holder> {
        let none = std::path::PathBuf::new();
        Arc::new(Holder {
            state: Mutex::new(State {
                terms: HashMap::new(),
                core,
                generation,
                previous: "",
                core_left: Instant::now(),
                restarts: Vec::new(),
            }),
            changed: Condvar::new(),
            instance: "test".into(),
            grace: Duration::ZERO,
            paths: Paths {
                socket: none.clone(),
                holder_socket: none.clone(),
                state: none.clone(),
                config: none.clone(),
            },
            socket: none,
            draining: std::sync::atomic::AtomicBool::new(false),
            sweeps: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// `len` bytes whose pattern depends on `seed`, so a swapped or cut frame shows.
    fn patterned(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    /// Queues `frames` through `to_core` (on top of `extra` bytes owed elsewhere)
    /// before the writer starts, so it has to batch them. Returns everything the
    /// reader got until the writer closed, and the backlog once it settled at `extra`.
    fn deliver(frames: &[Vec<u8>], extra: usize) -> (Vec<u8>, Option<usize>) {
        let (writer, mut reader) = linked();
        let (sender, frames_rx) = std::sync::mpsc::channel();
        let owed = backlog(Some(extra));
        let core = Core { generation: 1, frames: sender, backlog: owed.clone(), stream: writer.try_clone().unwrap() };
        let holder = test_holder(1, Some(core));
        {
            let mut st = holder.lock();
            for frame in frames {
                holder.to_core(&mut st, frame.clone());
            }
        }
        let reading = std::thread::spawn(move || {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).map(|_| got)
        });
        let (writing_holder, written) = (holder.clone(), owed.clone());
        let writing = std::thread::spawn(move || writing_holder.write_to_core(1, writer, frames_rx, written));
        owed.wait(extra, Duration::from_secs(30));
        let left = queued(&owed);
        // Closing the sender ends the writer, which closes the link for the reader.
        drop(holder.lock().core.take());
        writing.join().unwrap();
        (reading.join().unwrap().unwrap(), left)
    }

    #[test]
    fn batched_frames_of_mixed_sizes_reach_the_core_byte_for_byte_in_order() {
        let mut frames = vec![Vec::new(), patterned(1, 1), patterned(3, 2), Vec::new()];
        frames.extend((0..5000).map(|i| patterned(1 + i % 7, i as u8)));
        frames.extend([
            patterned(BATCH - 1, 3),
            patterned(2, 4),
            patterned(BATCH * 3 + 5, 5),
            Vec::new(),
            patterned(9, 6),
        ]);
        let (got, left) = deliver(&frames, 3);
        assert_eq!(got.len(), frames.iter().map(Vec::len).sum::<usize>());
        assert!(got == frames.concat(), "the core got the frames out of order or altered");
        assert_eq!(left, Some(3), "the backlog must drop by exactly what was written");
    }

    #[test]
    fn frames_on_the_batch_boundary_are_neither_split_nor_reordered() {
        let frames = [
            patterned(BATCH, 1),
            patterned(1, 2),
            patterned(BATCH + 1, 3),
            patterned(1, 4),
            patterned(BATCH - 1, 5),
            patterned(1, 6),
            patterned(1, 7),
        ];
        let (got, left) = deliver(&frames, 1);
        assert!(got == frames.concat(), "frames around 64 KiB arrived altered");
        assert_eq!(left, Some(1));
    }

    #[test]
    fn a_single_frame_far_over_the_batch_size_goes_out_whole() {
        let frames = [patterned(BACKLOG_BYTES + 1, 9), patterned(2, 1)];
        let (got, left) = deliver(&frames, 0);
        assert!(got == frames.concat(), "a huge frame was cut or reordered");
        assert_eq!(left, Some(0));
    }

    #[test]
    fn only_empty_frames_write_nothing_and_leave_the_backlog_as_it_was() {
        let (got, left) = deliver(&[Vec::new(), Vec::new(), Vec::new()], 2);
        assert!(got.is_empty());
        assert_eq!(left, Some(2));
    }

    #[test]
    fn the_shutdown_wait_sees_the_backlog_drain_to_zero() {
        let mut frames: Vec<Vec<u8>> = (0..200).map(|i| patterned(700 + i, i as u8)).collect();
        frames.push(proto::frame(proto::BYE, 0, &[]));
        let (got, left) = deliver(&frames, 0);
        assert_eq!(left, Some(0), "backlog.wait(0) before a shutdown must see zero");
        assert!(got.ends_with(&proto::frame(proto::BYE, 0, &[])), "BYE must arrive last");
        assert!(got == frames.concat());
    }

    #[test]
    fn a_lone_frame_is_written_without_waiting_for_more() {
        let (writer, mut reader) = linked();
        let (sender, frames_rx) = std::sync::mpsc::channel();
        let owed = backlog(Some(0));
        let core = Core { generation: 1, frames: sender, backlog: owed.clone(), stream: writer.try_clone().unwrap() };
        let holder = test_holder(1, Some(core));
        let (writing_holder, written) = (holder.clone(), owed.clone());
        let writing = std::thread::spawn(move || writing_holder.write_to_core(1, writer, frames_rx, written));
        // The writer is already waiting on an empty queue when the frame comes.
        std::thread::sleep(Duration::from_millis(50));
        holder.to_core(&mut holder.lock(), patterned(5, 1));
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut got = [0u8; 5];
            let _ = got_tx.send(reader.read_exact(&mut got).map(|_| got));
        });
        let got = got_rx.recv_timeout(Duration::from_secs(10)).expect("a lone frame must go out at once").unwrap();
        assert_eq!(got.to_vec(), patterned(5, 1));
        owed.wait(0, Duration::from_secs(10));
        assert_eq!(queued(&owed), Some(0));
        drop(holder.lock().core.take());
        writing.join().unwrap();
    }

    #[test]
    fn a_failed_write_drops_the_backlog_and_runs_the_crash_path() {
        let (writer, reader) = linked();
        drop(reader);
        let (sender, frames_rx) = std::sync::mpsc::channel();
        let owed = backlog(Some(0));
        let core = Core { generation: 1, frames: sender, backlog: owed.clone(), stream: writer.try_clone().unwrap() };
        let holder = test_holder(1, Some(core));
        {
            let mut st = holder.lock();
            for (i, len) in [10, BATCH, 1, BATCH + 1, 3].into_iter().enumerate() {
                holder.to_core(&mut st, patterned(len, i as u8));
            }
        }
        let (writing_holder, written) = (holder.clone(), owed.clone());
        // The sender stays open in the core: only the failed write can end the writer.
        let writing = std::thread::spawn(move || writing_holder.write_to_core(1, writer, frames_rx, written));
        owed.wait(0, Duration::from_secs(10));
        assert_eq!(queued(&owed), None, "a failed write must stop counting the backlog");
        writing.join().unwrap();
        let st = holder.lock();
        assert!(st.core.is_none(), "the core whose write failed must be gone");
        assert_eq!(st.previous, "crash");
    }

    #[test]
    fn a_stale_writer_failing_leaves_the_newer_core_alone() {
        let (writer, reader) = linked();
        drop(reader);
        let (newer_tx, _newer_rx) = std::sync::mpsc::channel();
        let (newer_stream, _newer_reader) = linked();
        let newer = Core { generation: 2, frames: newer_tx, backlog: backlog(Some(0)), stream: newer_stream };
        let holder = test_holder(2, Some(newer));
        let (stale_tx, stale_rx) = std::sync::mpsc::channel();
        stale_tx.send(patterned(BATCH + 1, 1)).unwrap();
        let owed = backlog(Some(BATCH + 1));
        let (writing_holder, written) = (holder.clone(), owed.clone());
        let writing = std::thread::spawn(move || writing_holder.write_to_core(1, writer, stale_rx, written));
        owed.wait(0, Duration::from_secs(10));
        assert_eq!(queued(&owed), None);
        writing.join().unwrap();
        let st = holder.lock();
        assert_eq!(st.core.as_ref().map(|c| c.generation), Some(2));
        assert_eq!(st.previous, "");
        drop(stale_tx);
    }

    #[test]
    fn frames_queued_before_a_handover_are_all_written() {
        let (writer, mut reader) = linked();
        let (sender, frames_rx) = std::sync::mpsc::channel();
        let owed = backlog(Some(0));
        let core = Core { generation: 1, frames: sender, backlog: owed.clone(), stream: writer.try_clone().unwrap() };
        let holder = test_holder(1, Some(core));
        let mut frames: Vec<Vec<u8>> = (0..300).map(|i| patterned(1 + i * 37 % 2000, i as u8)).collect();
        frames.extend([Vec::new(), patterned(BATCH + 7, 7), proto::frame(proto::BYE, 0, &[])]);
        {
            let mut st = holder.lock();
            for frame in &frames {
                holder.to_core(&mut st, frame.clone());
            }
            // Drops the core's sender: everything is queued before the writer even starts.
            holder.core_gone(&mut st, "handover");
        }
        assert_eq!(queued(&owed), None, "a handover no longer holds programs back");
        let (writing_holder, written) = (holder.clone(), owed);
        let writing = std::thread::spawn(move || writing_holder.write_to_core(1, writer, frames_rx, written));
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        writing.join().unwrap();
        assert!(got == frames.concat(), "frames queued before the handover were lost or reordered");
        assert_eq!(holder.lock().previous, "handover", "a clean end must not count as a crash");
    }
}
