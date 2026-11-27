//! Read-only foreground evidence from KDE's unofficial Plasma Wayland protocol.

use rustix::{
	event::{PollFd, PollFlags, Timespec, poll},
	net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with},
};
use std::{
	collections::HashMap,
	env, fs,
	io::Read,
	os::{
		fd::{AsFd, AsRawFd},
		unix::{ffi::OsStrExt, net::UnixStream},
	},
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};
use wayland_client::{
	Connection, Dispatch, Proxy, QueueHandle,
	backend::ObjectId,
	protocol::{wl_callback, wl_registry},
};
use wayland_protocols_plasma::plasma_window_management::client::{
	org_kde_plasma_window::{self as window, OrgKdePlasmaWindow},
	org_kde_plasma_window_management::{self as management, OrgKdePlasmaWindowManagement},
};

const MIN_VERSION: u32 = 13; // WindowWithUuid
const MAX_WINDOWS: usize = 256;
const MAX_DISPATCHED: usize = 4096;
const SYNC_TIMEOUT: Duration = Duration::from_millis(100);
const REFRESH: Duration = Duration::from_millis(150);
const LEASE: Duration = Duration::from_millis(300);
const ACTIVE: u32 = management::State::Active as u32;
const NOT_VISIBLE: u32 = management::State::Minimized as u32 | management::State::Shaded as u32;
// Toggling notifications can replace the observer while a previous native frame still exists.
static GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct Evidence {
	executable: Arc<Path>,
	valid_until: Instant,
	generation: u64,
}

impl Evidence {
	pub fn executable(&self) -> &Path {
		&self.executable
	}

	pub fn valid_until(&self) -> Instant {
		self.valid_until
	}

	/// Stable across uninterrupted renewals of the same observation.
	pub fn generation(&self) -> u64 {
		self.generation
	}
}

type Latest = Arc<Mutex<Option<Evidence>>>;

pub struct ForegroundWindow {
	latest: Latest,
	stop: Option<UnixStream>,
	thread: Option<JoinHandle<()>>,
}

impl ForegroundWindow {
	/// Start a separate client; `current` is empty until the compositor is verified.
	/// `wake` must be nonblocking and is called for loss and acquisition, not routine renewals.
	pub fn start(wake: impl Fn() + Send + 'static) -> Option<Self> {
		let path = display_path()?;
		let latest = Latest::default();
		let (stop, stopped) = UnixStream::pair().ok()?;
		let thread = {
			let latest = latest.clone();
			thread::Builder::new()
				.name("foreground-window".into())
				.spawn(move || {
					let mut observer = Observer {
						latest,
						wake: Box::new(wake),
						..Observer::default()
					};
					let _ = observe(&path, &stopped, &mut observer);
					observer.clear();
				})
				.ok()?
		};
		Some(Self {
			latest,
			stop: Some(stop),
			thread: Some(thread),
		})
	}

	pub fn current(&self, now: Instant) -> Option<Evidence> {
		current(&self.latest, now)
	}
}

impl Drop for ForegroundWindow {
	fn drop(&mut self) {
		drop(self.stop.take()); // Hang up the worker's poll fd, even if the display never replies.
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

fn current(latest: &Latest, now: Instant) -> Option<Evidence> {
	latest
		.lock()
		.ok()?
		.as_ref()
		.filter(|e| now < e.valid_until)
		.cloned()
}

fn display_path() -> Option<PathBuf> {
	// Do not adopt WAYLAND_SOCKET: it may belong to the main client.
	let name = PathBuf::from(env::var_os("WAYLAND_DISPLAY")?);
	if name.is_absolute() {
		return Some(name);
	}
	let runtime = PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?);
	(runtime.is_absolute() && !name.as_os_str().is_empty()).then(|| runtime.join(name))
}

#[derive(Clone, Copy)]
struct Process {
	pid: u32,
	started: u64,
}

impl Process {
	fn validate(
		self,
		mut start_time: impl FnMut(u32) -> Option<u64>,
		executable: impl FnOnce(u32) -> Option<PathBuf>,
	) -> Option<PathBuf> {
		if start_time(self.pid)? != self.started {
			return None;
		}
		let path = executable(self.pid)?;
		(path.is_absolute()
			&& !path.as_os_str().as_bytes().ends_with(b" (deleted)")
			&& start_time(self.pid)? == self.started)
			.then_some(path)
	}
}

fn read_proc(path: impl AsRef<Path>) -> Option<Vec<u8>> {
	let mut bytes = Vec::new();
	fs::File::open(path)
		.ok()?
		.take(8193)
		.read_to_end(&mut bytes)
		.ok()?;
	(bytes.len() <= 8192).then_some(bytes)
}

fn proc_start_time(pid: u32) -> Option<u64> {
	parse_proc_start_time(&read_proc(format!("/proc/{pid}/stat"))?)
}

fn procfs_covers_local_pids(status: &[u8], pid: u32) -> bool {
	let Some(namespace_pids) = status
		.split(|&b| b == b'\n')
		.find_map(|line| line.strip_prefix(b"NSpid:"))
	else {
		return false;
	};
	let mut ids = namespace_pids
		.split(u8::is_ascii_whitespace)
		.filter(|field| !field.is_empty());
	ids.next() == Some(pid.to_string().as_bytes()) && ids.next().is_none()
}

#[allow(unsafe_code)]
fn compositor_pid(socket: &impl AsFd) -> Option<u32> {
	let mut credentials = libc::ucred {
		pid: 0,
		uid: 0,
		gid: 0,
	};
	let size: libc::socklen_t = std::mem::size_of_val(&credentials).try_into().ok()?;
	let mut len = size;
	// SAFETY: The borrowed socket remains live and both output pointers refer to writable
	// storage of the stated size, using the Linux SO_PEERCRED ABI.
	let result = unsafe {
		libc::getsockopt(
			socket.as_fd().as_raw_fd(),
			libc::SOL_SOCKET,
			libc::SO_PEERCRED,
			(&mut credentials as *mut libc::ucred).cast(),
			&mut len,
		)
	};
	// Linux may return pid 0 if the peer is outside the caller's PID namespace.
	(result == 0 && len == size && credentials.pid > 0).then_some(credentials.pid as u32)
}

fn shared_pid_namespace(socket: &impl AsFd) -> Option<()> {
	let peer = compositor_pid(socket)?;
	// A host procfs mount inside a PID namespace can contain unrelated processes
	// with the same numeric IDs. Require procfs, this client and the peer to agree.
	if !procfs_covers_local_pids(&read_proc("/proc/self/status")?, std::process::id()) {
		return None;
	}
	(fs::read_link("/proc/self/ns/pid").ok()?
		== fs::read_link(format!("/proc/{peer}/ns/pid")).ok()?)
	.then_some(())
}

fn parse_proc_start_time(stat: &[u8]) -> Option<u64> {
	// Field 2 (comm) can contain whitespace and parentheses. Start time is field 22.
	let end_of_comm = stat.iter().rposition(|&byte| byte == b')')?;
	let start = stat[end_of_comm + 1..]
		.split(u8::is_ascii_whitespace)
		.filter(|field| !field.is_empty())
		.nth(19)?;
	std::str::from_utf8(start).ok()?.parse().ok()
}

#[derive(Default)]
struct WindowState {
	initialized: bool,
	flags: Option<u32>,
	pid: Option<u32>,
	started: Option<u64>,
}

impl WindowState {
	fn set_pid(&mut self, pid: u32) -> bool {
		if self.pid == Some(pid) {
			return false;
		}
		self.pid = Some(pid);
		// Pin the lifetime when the compositor first reports this PID, not when it gains focus.
		self.started = (1..=i32::MAX as u32)
			.contains(&pid)
			.then(|| proc_start_time(pid))
			.flatten();
		true
	}
}

struct Observer {
	manager: Option<u32>,
	windows: HashMap<ObjectId, WindowState>,
	showing_desktop: bool,
	revision: u64,
	remaining_events: usize,
	sync_done: bool,
	failed: bool,
	latest: Latest,
	wake: Box<dyn Fn() + Send>,
}

impl Default for Observer {
	fn default() -> Self {
		Self {
			manager: None,
			windows: HashMap::new(),
			showing_desktop: false,
			revision: 0,
			remaining_events: MAX_DISPATCHED,
			sync_done: false,
			failed: false,
			latest: Latest::default(),
			wake: Box::new(|| {}),
		}
	}
}

impl Observer {
	fn clear(&self) {
		let had_evidence = self.latest.lock().unwrap().take().is_some();
		if had_evidence {
			(self.wake)();
		}
	}

	fn changed(&mut self) {
		self.revision += 1;
		self.clear();
	}

	fn fail(&mut self) {
		self.failed = true;
		self.clear();
	}

	fn permit_event(&mut self) -> bool {
		if self.failed {
			return false;
		}
		if self.remaining_events == 0 {
			self.fail();
			return false;
		}
		self.remaining_events -= 1;
		true
	}

	fn show_desktop(&mut self, value: bool) {
		if std::mem::replace(&mut self.showing_desktop, value) != value {
			self.changed();
		}
	}

	fn publish(&mut self, path: PathBuf, now: Instant, valid_until: Instant) {
		let mut latest = self.latest.lock().unwrap();
		if let Some(e) = latest
			.as_mut()
			.filter(|e| now < e.valid_until && e.executable.as_ref() == path.as_path())
		{
			e.valid_until = valid_until;
			return;
		}
		*latest = Some(Evidence {
			executable: path.into(),
			valid_until,
			generation: GENERATION.fetch_add(1, Ordering::Relaxed),
		});
		drop(latest);
		(self.wake)();
	}
}

fn active_process<'a>(
	showing_desktop: bool,
	windows: impl IntoIterator<Item = &'a WindowState>,
) -> Option<Process> {
	if showing_desktop {
		return None;
	}
	let mut active = None;
	for window in windows {
		if !window.initialized {
			return None;
		}
		let flags = window.flags?;
		if flags & ACTIVE != 0 {
			if active.is_some() || flags & NOT_VISIBLE != 0 {
				return None;
			}
			active = Some(Process {
				pid: window.pid?,
				started: window.started?,
			});
		}
	}
	active
}

struct Barrier {
	requested: Instant,
	candidate: Option<(u64, Process)>,
}

fn observe(path: &Path, stopped: &UnixStream, state: &mut Observer) -> Option<()> {
	let socket = socket_with(
		AddressFamily::UNIX,
		SocketType::STREAM,
		SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
		None,
	)
	.ok()?;
	connect(&socket, &SocketAddrUnix::new(path).ok()?).ok()?;
	shared_pid_namespace(&socket)?;
	let conn = Connection::from_socket(UnixStream::from(socket)).ok()?;
	let mut queue = conn.new_event_queue();
	let handle = queue.handle();
	let display = conn.display();
	let _registry = display.get_registry(&handle, ());
	let mut barrier = Some(Barrier {
		requested: Instant::now(),
		candidate: None,
	});
	display.sync(&handle, ());
	let mut last_request = None;

	loop {
		// The library still drains the batch; callbacks stop doing work at this limit.
		state.remaining_events = MAX_DISPATCHED;
		if queue.dispatch_pending(state).is_err() || state.failed {
			return None;
		}
		// Any write backpressure is a loss of reliable evidence for this client.
		queue.flush().ok()?;
		let now = Instant::now();
		if let Some(in_flight) = &barrier {
			if now.duration_since(in_flight.requested) >= SYNC_TIMEOUT {
				return None;
			}
			if std::mem::take(&mut state.sync_done) {
				let finished = barrier.take()?;
				match finished.candidate {
					// The bootstrap barrier proves registry discovery, not window state.
					None => {
						state.manager?;
					}
					Some((revision, process)) if revision == state.revision => {
						let path = process.validate(proc_start_time, |pid| {
							fs::read_link(format!("/proc/{pid}/exe")).ok()
						});
						let verified = Instant::now();
						if let Some(path) = path
							.filter(|_| verified.duration_since(finished.requested) < SYNC_TIMEOUT)
						{
							state.publish(path, verified, finished.requested + LEASE);
						} else {
							state.clear();
						}
					}
					_ => {}
				}
			}
		}

		let mut deadline = None;
		if barrier.is_none()
			&& let Some(process) = active_process(state.showing_desktop, state.windows.values())
		{
			let now = Instant::now();
			deadline = last_request
				.filter(|(revision, _)| *revision == state.revision)
				.map(|(_, requested)| requested + REFRESH);
			if deadline.is_none_or(|at| now >= at) {
				display.sync(&handle, ());
				barrier = Some(Barrier {
					requested: now,
					candidate: Some((state.revision, process)),
				});
				last_request = Some((state.revision, now));
				queue.flush().ok()?;
			}
		}
		if let Some(in_flight) = &barrier {
			deadline = Some(in_flight.requested + SYNC_TIMEOUT);
		}
		let timeout = deadline
			.map(|at| Timespec::try_from(at.saturating_duration_since(Instant::now())).unwrap());
		let Some(read) = queue.prepare_read() else {
			continue;
		};
		let display_fd = read.connection_fd();
		let mut fds = [
			PollFd::new(stopped, PollFlags::IN),
			PollFd::new(&display_fd, PollFlags::IN),
		];
		match poll(&mut fds, timeout.as_ref()) {
			Ok(_) => {}
			Err(rustix::io::Errno::INTR) => continue,
			Err(_) => return None,
		}
		if !fds[0].revents().is_empty() {
			return Some(());
		}
		let events = fds[1].revents();
		if events.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
			return None;
		}
		if events.contains(PollFlags::IN) {
			read.read().ok()?;
		}
	}
}

impl Dispatch<wl_registry::WlRegistry, ()> for Observer {
	fn event(
		observer: &mut Self,
		registry: &wl_registry::WlRegistry,
		event: wl_registry::Event,
		_: &(),
		_: &Connection,
		handle: &QueueHandle<Self>,
	) {
		if !observer.permit_event() {
			return;
		}
		match event {
			wl_registry::Event::Global {
				name,
				interface,
				version,
			} if interface == OrgKdePlasmaWindowManagement::interface().name => {
				if version < MIN_VERSION || observer.manager.replace(name).is_some() {
					observer.fail();
					return;
				}
				let version = version.min(OrgKdePlasmaWindowManagement::interface().version);
				registry.bind::<OrgKdePlasmaWindowManagement, _, _>(name, version, handle, ());
			}
			wl_registry::Event::GlobalRemove { name } if observer.manager == Some(name) => {
				observer.fail()
			}
			_ => {}
		}
	}
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for Observer {
	fn event(
		observer: &mut Self,
		manager: &OrgKdePlasmaWindowManagement,
		event: management::Event,
		_: &(),
		_: &Connection,
		handle: &QueueHandle<Self>,
	) {
		if !observer.permit_event() {
			return;
		}
		match event {
			management::Event::ShowDesktopChanged { state } => match state {
				0 => observer.show_desktop(false),
				1 => observer.show_desktop(true),
				_ => observer.fail(),
			},
			management::Event::WindowWithUuid { uuid, .. }
				if !uuid.is_empty()
					&& uuid.len() <= 128
					&& observer.windows.len() < MAX_WINDOWS =>
			{
				let window = manager.get_window_by_uuid(uuid, handle, ());
				observer.windows.insert(window.id(), WindowState::default());
				observer.changed();
			}
			management::Event::WindowWithUuid { .. } | management::Event::Window { .. } => {
				observer.fail()
			}
			_ => {}
		}
	}
}

impl Dispatch<OrgKdePlasmaWindow, ()> for Observer {
	fn event(
		observer: &mut Self,
		window: &OrgKdePlasmaWindow,
		event: window::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if !observer.permit_event() {
			return;
		}
		let id = window.id();
		let Some(state) = observer.windows.get_mut(&id) else {
			return;
		};
		let changed = match event {
			window::Event::StateChanged { flags } => {
				let flags = flags & (ACTIVE | NOT_VISIBLE);
				state.flags.replace(flags) != Some(flags)
			}
			window::Event::PidChanged { pid } => state.set_pid(pid),
			window::Event::InitialState => !std::mem::replace(&mut state.initialized, true),
			window::Event::Unmapped => {
				observer.windows.remove(&id);
				window.destroy();
				true
			}
			// Never use or retain the title, app ID or other descriptive metadata.
			_ => false,
		};
		if changed {
			observer.changed();
		}
	}
}

impl Dispatch<wl_callback::WlCallback, ()> for Observer {
	fn event(
		observer: &mut Self,
		_: &wl_callback::WlCallback,
		_: wl_callback::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if observer.permit_event() {
			observer.sync_done = true;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{cell::Cell, sync::atomic::AtomicUsize};

	#[test]
	fn active_process_requires_initialized_unambiguous_focus_and_pid() {
		let active = |pid, started| WindowState {
			initialized: true,
			flags: Some(ACTIVE),
			pid,
			started,
		};
		let game = active(Some(42), Some(7));
		let pending = WindowState::default();
		let other = active(Some(53), Some(8));
		let missing_pid = active(Some(0), None);
		assert!(active_process(false, [&game, &pending]).is_none());
		assert!(active_process(false, [&game, &other]).is_none());
		assert!(active_process(false, [&missing_pid]).is_none());
		// If the other window is unmapped the remaining initialized window can qualify.
		assert_eq!(active_process(false, [&game]).map(|p| p.pid), Some(42));
		assert!(active_process(true, [&game]).is_none());
		let minimized = WindowState {
			flags: Some(ACTIVE | management::State::Minimized as u32),
			..game
		};
		assert!(active_process(false, [&minimized]).is_none());
	}

	#[test]
	fn proc_start_time_ignores_parentheses_and_whitespace_in_comm() {
		assert_eq!(
			parse_proc_start_time(
				b"42 (odd) comm\nname) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 623 20"
			),
			Some(623)
		);
		assert_eq!(parse_proc_start_time(b"42 (comm) S 1 2 3"), None);
	}

	#[test]
	fn procfs_pid_mapping_refuses_an_ancestor_mount_even_with_equal_ids() {
		assert!(procfs_covers_local_pids(b"Name:\tserein\nNSpid:\t42\n", 42));
		assert!(!procfs_covers_local_pids(
			b"Name:\tserein\nNSpid:\t42\t42\n",
			42
		));
		assert!(!procfs_covers_local_pids(b"Name:\tserein\n", 42));
	}

	#[test]
	fn executable_validation_rejects_pid_reuse_during_lookup() {
		let process = Process {
			pid: 42,
			started: 7,
		};
		let started = Cell::new(7);
		assert!(
			process
				.validate(
					|_| Some(started.get()),
					|_| {
						started.set(8);
						Some("/games/example".into())
					}
				)
				.is_none()
		);
		assert!(
			process
				.validate(|_| Some(7), |_| Some("example".into()))
				.is_none()
		);
	}

	#[test]
	fn renewal_does_not_wake_ui_but_showing_desktop_invalidates_evidence() {
		let wakes = Arc::new(AtomicUsize::new(0));
		let counter = wakes.clone();
		let mut observer = Observer {
			wake: Box::new(move || {
				counter.fetch_add(1, Ordering::Relaxed);
			}),
			..Observer::default()
		};
		let now = Instant::now();
		observer.publish("/games/example".into(), now, now + LEASE);
		let generation = current(&observer.latest, now).unwrap().generation();
		assert!(current(&observer.latest, now + LEASE).is_none());
		observer.publish(
			"/games/example".into(),
			now + REFRESH,
			now + REFRESH + LEASE,
		);
		assert_eq!(
			current(&observer.latest, now + LEASE).unwrap().generation(),
			generation
		);
		assert_eq!(wakes.load(Ordering::Relaxed), 1);
		observer.show_desktop(true);
		assert!(current(&observer.latest, now + LEASE).is_none());
		assert_eq!(wakes.load(Ordering::Relaxed), 2);
		let mut restarted = Observer::default();
		restarted.publish("/games/example".into(), now, now + LEASE);
		assert_ne!(
			current(&restarted.latest, now).unwrap().generation(),
			generation
		);
	}
}
