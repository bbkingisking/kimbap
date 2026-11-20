//! The control thread is the only caller of blocking libmpv client APIs.
//! GTK only uses mpv_render_* on its current GL context.
use libmpv2_sys as raw;
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString, c_void},
    path::PathBuf,
    ptr::{self, NonNull},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub struct Core {
    pub handle: NonNull<raw::mpv_handle>,
    wake: Arc<Queue>,
}
// libmpv client handles are thread-safe. Only the control thread uses the client
// API; the GTK renderer retains an Arc solely to keep the handle alive.
unsafe impl Send for Core {}
unsafe impl Sync for Core {}
impl Drop for Core {
    fn drop(&mut self) {
        unsafe {
            raw::mpv_set_wakeup_callback(self.handle.as_ptr(), None, ptr::null_mut());
            raw::mpv_terminate_destroy(self.handle.as_ptr());
        }
    }
}
fn error(code: i32) -> String {
    unsafe { CStr::from_ptr(raw::mpv_error_string(code)) }
        .to_string_lossy()
        .into_owned()
}
fn check(code: i32) -> Result<(), String> {
    if code < 0 { Err(error(code)) } else { Ok(()) }
}
unsafe extern "C" fn wakeup(data: *mut c_void) {
    let queue = unsafe { &*(data as *const Queue) };
    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
    state.serial = state.serial.wrapping_add(1);
    queue.wake.notify_one();
}
impl Core {
    fn new(queue: Arc<Queue>, mode: &str, ao: Option<&str>) -> Result<Arc<Self>, String> {
        let handle = NonNull::new(unsafe { raw::mpv_create() }).ok_or("Cannot allocate libmpv")?;
        let core = Self {
            handle,
            wake: queue,
        };
        let mut options = vec![
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("terminal", "no"),
            ("vo", "libmpv"),
            ("idle", "yes"),
            ("keep-open", "yes"),
            ("reset-on-next-file", "pause"),
            ("loop-file", "no"),
            ("audio-display", "no"),
            ("osd-level", "0"),
            ("access-references", "no"),
            ("hwdec", mode),
            ("hwdec-codecs", "all"),
            ("video-timing-offset", "0"),
            ("mute", "yes"),
            ("audio-client-name", "Kimbap"),
        ];
        if let Some(ao) = ao {
            options.push(("ao", ao));
        }
        for (name, value) in options {
            let name = CString::new(name).unwrap();
            let value = CString::new(value).map_err(|e| e.to_string())?;
            check(unsafe {
                raw::mpv_set_option_string(handle.as_ptr(), name.as_ptr(), value.as_ptr())
            })
            .map_err(|e| format!("mpv option {name:?}: {e}"))?;
        }
        check(unsafe { raw::mpv_initialize(handle.as_ptr()) })?;
        check(unsafe { raw::mpv_request_log_messages(handle.as_ptr(), c"warn".as_ptr()) })?;
        unsafe {
            raw::mpv_set_wakeup_callback(
                handle.as_ptr(),
                Some(wakeup),
                Arc::as_ptr(&core.wake).cast_mut().cast(),
            )
        };
        Ok(Arc::new(core))
    }
    fn command(&self, args: &[CString]) -> Result<(), String> {
        let mut pointers: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
        pointers.push(ptr::null());
        check(unsafe { raw::mpv_command(self.handle.as_ptr(), pointers.as_mut_ptr()) })
    }
    fn stop(&self) {
        let _ = self.command(&[CString::new("stop").unwrap()]);
    }
    fn property<T: Default>(&self, name: &CStr, format: raw::mpv_format) -> Option<T> {
        let mut value = T::default();
        let result = unsafe {
            raw::mpv_get_property(
                self.handle.as_ptr(),
                name.as_ptr(),
                format,
                (&mut value as *mut T).cast(),
            )
        };
        (result >= 0).then_some(value)
    }
    fn int(&self, name: &CStr) -> Option<i64> {
        self.property(name, raw::mpv_format_MPV_FORMAT_INT64)
    }
    fn double(&self, name: &CStr) -> Option<f64> {
        self.property(name, raw::mpv_format_MPV_FORMAT_DOUBLE)
    }
    fn flag(&self, name: &CStr) -> Option<bool> {
        self.property::<i32>(name, raw::mpv_format_MPV_FORMAT_FLAG)
            .map(|v| v != 0)
    }
    fn string(&self, name: &CStr) -> Option<String> {
        let ptr = unsafe { raw::mpv_get_property_string(self.handle.as_ptr(), name.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let value = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { raw::mpv_free(ptr.cast()) };
        Some(value)
    }
    fn muted(&self, muted: bool) -> Result<(), String> {
        let mut value = i32::from(muted);
        check(unsafe {
            raw::mpv_set_property(
                self.handle.as_ptr(),
                c"mute".as_ptr(),
                raw::mpv_format_MPV_FORMAT_FLAG,
                (&mut value as *mut i32).cast(),
            )
        })
    }
}
#[derive(Clone)]
pub struct Target {
    pub generation: u64,
    pub path: PathBuf,
}
#[derive(Default)]
struct State {
    target: Option<Option<Target>>,
    muted: Option<bool>,
    seek: Option<(u64, f64)>,
    paused: Option<(u64, bool)>,
    renderer: bool,
    shutdown: bool,
    serial: u64,
}
pub struct Queue {
    state: Mutex<State>,
    wake: Condvar,
}
pub struct Control {
    queue: Arc<Queue>,
    thread: Option<thread::JoinHandle<()>>,
}
#[derive(Debug)]
pub struct Stats {
    pub generation: u64,
    pub position: f64,
    pub duration: f64,
    pub codec: String,
    pub hwdec: String,
    pub dropped: i64,
    pub decoder_dropped: i64,
    pub eof: bool,
    pub paused: Option<bool>,
    pub paused_for_cache: Option<bool>,
    pub muted: bool,
}
pub enum Signal {
    Core(Arc<Core>),
    Progress {
        generation: u64,
        position: f64,
        duration: f64,
        seekable: bool,
        paused: bool,
    },
    Info {
        generation: u64,
        width: i32,
        height: i32,
    },
    Failed {
        generation: u64,
        message: String,
    },
    Stats(Stats),
}
impl Control {
    pub fn new(mode: String, ao: Option<String>) -> (Self, async_channel::Receiver<Signal>) {
        let queue = Arc::new(Queue {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        let (send, recv) = async_channel::unbounded();
        let state = queue.clone();
        let thread = thread::Builder::new()
            .name("libmpv-control".into())
            .spawn(move || run(state, send, mode, ao))
            .expect("create libmpv control thread");
        (
            Self {
                queue,
                thread: Some(thread),
            },
            recv,
        )
    }
    pub fn load(&self, generation: u64, path: PathBuf) {
        let mut state = self.queue.state.lock().unwrap();
        state.target = Some(Some(Target { generation, path }));
        state.serial = state.serial.wrapping_add(1);
        self.queue.wake.notify_one();
    }
    pub fn stop(&self) {
        let mut state = self.queue.state.lock().unwrap();
        state.target = Some(None);
        state.serial = state.serial.wrapping_add(1);
        self.queue.wake.notify_one();
    }
    pub fn mute(&self, muted: bool) {
        let mut state = self.queue.state.lock().unwrap();
        state.muted = Some(muted);
        state.serial = state.serial.wrapping_add(1);
        self.queue.wake.notify_one();
    }
    pub fn pause(&self, generation: u64, paused: bool) {
        let mut state = self.queue.state.lock().unwrap();
        state.paused = Some((generation, paused));
        state.serial = state.serial.wrapping_add(1);
        self.queue.wake.notify_one();
    }
    pub fn seek(&self, generation: u64, fraction: f64) {
        if !fraction.is_finite() {
            return;
        }
        let mut state = self.queue.state.lock().unwrap();
        // Coalesce drag motion so the control thread always seeks to the latest point.
        state.seek = Some((generation, fraction.clamp(0., 1.)));
        state.serial = state.serial.wrapping_add(1);
        self.queue.wake.notify_one();
    }
    pub fn renderer(&self, ready: bool) {
        let mut state = self.queue.state.lock().unwrap();
        state.renderer = ready;
        state.serial = state.serial.wrapping_add(1);
        self.queue.wake.notify_one();
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        state.shutdown = true;
        self.queue.wake.notify_one();
        drop(state);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn emit(send: &async_channel::Sender<Signal>, signal: Signal) {
    let _ = send.try_send(signal);
}
fn run(queue: Arc<Queue>, send: async_channel::Sender<Signal>, mode: String, ao: Option<String>) {
    let core = match Core::new(queue.clone(), &mode, ao.as_deref()) {
        Ok(c) => c,
        Err(message) => {
            emit(
                &send,
                Signal::Failed {
                    generation: 0,
                    message,
                },
            );
            return;
        }
    };
    emit(&send, Signal::Core(core.clone()));
    let mut entries = HashMap::new();
    let mut order = VecDeque::new();
    let mut active = None;
    let mut wanted: Option<Target> = None;
    let mut loaded = None;
    let mut next_stats = Instant::now();
    let mut next_progress = Instant::now();
    loop {
        let (target, muted, seek, paused, renderer, serial) = {
            let mut state = queue.state.lock().unwrap();
            if state.shutdown {
                break;
            }
            (
                state.target.take(),
                state.muted.take(),
                state.seek.take(),
                state.paused.take(),
                state.renderer,
                state.serial,
            )
        };
        if let Some(muted) = muted
            && let Err(message) = core.muted(muted)
        {
            emit(
                &send,
                Signal::Failed {
                    generation: wanted.as_ref().map(|t| t.generation).unwrap_or(0),
                    message,
                },
            );
        }
        if let Some(target) = target {
            wanted = target;
            loaded = None;
            log::debug!(
                "mpv target generation={:?} renderer_ready={renderer}",
                wanted.as_ref().map(|t| t.generation)
            );
            core.stop();
        }
        if let Some((generation, paused)) = paused
            && wanted.as_ref().map(|t| t.generation) == Some(generation)
            && loaded == Some(generation)
            && active.and_then(|id| entries.get(&id)).copied() == Some(generation)
        {
            // keep-open pauses on the last frame; Play should replay the clip.
            let result = (|| {
                if !paused && core.flag(c"eof-reached").unwrap_or(false) {
                    core.command(&[
                        CString::new("seek").unwrap(),
                        CString::new("0").unwrap(),
                        CString::new("absolute+exact").unwrap(),
                    ])?;
                }
                core.command(&[
                    CString::new("set").unwrap(),
                    CString::new("pause").unwrap(),
                    CString::new(if paused { "yes" } else { "no" }).unwrap(),
                ])
            })();
            if let Err(message) = result {
                log::warn!("video pause failed: {message}");
            }
        }
        if let Some((generation, fraction)) = seek
            && wanted.as_ref().map(|t| t.generation) == Some(generation)
            && loaded == Some(generation)
            && active.and_then(|id| entries.get(&id)).copied() == Some(generation)
        {
            let result = core.command(&[
                CString::new("seek").unwrap(),
                CString::new((fraction * 100.).to_string()).unwrap(),
                CString::new("absolute-percent+exact").unwrap(),
            ]);
            if let Err(message) = result {
                log::warn!("video seek failed: {message}");
            }
        }
        if renderer
            && let Some(target) = &wanted
            && loaded != Some(target.generation)
        {
            let path = match CString::new(target.path.as_os_str().as_encoded_bytes()) {
                Ok(p) => p,
                Err(e) => {
                    emit(
                        &send,
                        Signal::Failed {
                            generation: target.generation,
                            message: e.to_string(),
                        },
                    );
                    wanted = None;
                    continue;
                }
            };
            log::debug!("mpv loadfile generation={}", target.generation);
            match core.command(&[
                CString::new("loadfile").unwrap(),
                path,
                CString::new("replace").unwrap(),
            ]) {
                Ok(()) => {
                    if let Some(entry) = core.int(c"playlist/0/id") {
                        entries.insert(entry, target.generation);
                        order.push_back(entry);
                        while order.len() > 32 {
                            entries.remove(&order.pop_front().unwrap());
                        }
                        loaded = Some(target.generation);
                    } else {
                        emit(
                            &send,
                            Signal::Failed {
                                generation: target.generation,
                                message: "mpv did not assign a playlist entry".into(),
                            },
                        );
                        wanted = None;
                    }
                }
                Err(message) => {
                    emit(
                        &send,
                        Signal::Failed {
                            generation: target.generation,
                            message,
                        },
                    );
                    wanted = None;
                }
            }
        }
        loop {
            let event = unsafe { &*raw::mpv_wait_event(core.handle.as_ptr(), 0.) };
            if event.event_id == raw::mpv_event_id_MPV_EVENT_NONE {
                break;
            }
            match event.event_id {
                raw::mpv_event_id_MPV_EVENT_START_FILE => {
                    let data = unsafe { &*event.data.cast::<raw::mpv_event_start_file>() };
                    active = Some(data.playlist_entry_id);
                    log::debug!(
                        "mpv start-file entry={} generation={:?}",
                        data.playlist_entry_id,
                        entries.get(&data.playlist_entry_id)
                    );
                }
                // FILE_LOADED can still expose the previous VO dimensions while the
                // replacement decoder initializes. Wait until playback restarts.
                raw::mpv_event_id_MPV_EVENT_PLAYBACK_RESTART => {
                    if let Some(generation) = active.and_then(|id| entries.get(&id)).copied() {
                        let w = core.int(c"dwidth").unwrap_or(0);
                        let h = core.int(c"dheight").unwrap_or(0);
                        log::debug!("mpv playback-restart generation={generation} size={w}x{h}");
                        if w > 0 && h > 0 {
                            emit(
                                &send,
                                Signal::Info {
                                    generation,
                                    width: w as i32,
                                    height: h as i32,
                                },
                            );
                        }
                    }
                }
                raw::mpv_event_id_MPV_EVENT_END_FILE => {
                    let data = unsafe { &*event.data.cast::<raw::mpv_event_end_file>() };
                    let reason = match data.reason {
                        raw::mpv_end_file_reason_MPV_END_FILE_REASON_EOF => "eof",
                        raw::mpv_end_file_reason_MPV_END_FILE_REASON_STOP => "stop",
                        raw::mpv_end_file_reason_MPV_END_FILE_REASON_QUIT => "quit",
                        raw::mpv_end_file_reason_MPV_END_FILE_REASON_ERROR => "error",
                        raw::mpv_end_file_reason_MPV_END_FILE_REASON_REDIRECT => "redirect",
                        _ => "unknown",
                    };
                    log::debug!(
                        "mpv end-file entry={} generation={:?} reason={reason} error={}",
                        data.playlist_entry_id,
                        entries.get(&data.playlist_entry_id),
                        data.error
                    );
                    if active == Some(data.playlist_entry_id) {
                        active = None;
                    }
                    if data.reason == raw::mpv_end_file_reason_MPV_END_FILE_REASON_ERROR
                        && let Some(generation) = entries.get(&data.playlist_entry_id)
                    {
                        emit(
                            &send,
                            Signal::Failed {
                                generation: *generation,
                                message: format!("Video playback failed: {}", error(data.error)),
                            },
                        );
                    }
                }
                raw::mpv_event_id_MPV_EVENT_LOG_MESSAGE => {
                    let data = unsafe { &*event.data.cast::<raw::mpv_event_log_message>() };
                    let prefix = unsafe { CStr::from_ptr(data.prefix) }.to_string_lossy();
                    let text = unsafe { CStr::from_ptr(data.text) }.to_string_lossy();
                    eprint!("kimbap mpv [{prefix}]: {text}");
                }
                raw::mpv_event_id_MPV_EVENT_QUEUE_OVERFLOW => {
                    eprintln!("kimbap: mpv event queue overflow");
                }
                raw::mpv_event_id_MPV_EVENT_SHUTDOWN => {
                    log::debug!("mpv shutdown");
                    return;
                }
                _ => (),
            }
        }
        if Instant::now() >= next_progress {
            next_progress = Instant::now() + Duration::from_millis(100);
            if let Some(generation) = active.and_then(|id| entries.get(&id)).copied() {
                emit(
                    &send,
                    Signal::Progress {
                        generation,
                        position: core.double(c"time-pos").unwrap_or(0.),
                        duration: core.double(c"duration").unwrap_or(0.),
                        seekable: core.flag(c"seekable").unwrap_or(false),
                        paused: core.flag(c"pause").unwrap_or(false),
                    },
                );
            }
        }
        if log::log_enabled!(target: "kimbap::video", log::Level::Debug)
            && Instant::now() >= next_stats
        {
            next_stats = Instant::now() + Duration::from_secs(1);
            if let Some(generation) = active.and_then(|id| entries.get(&id)).copied() {
                emit(
                    &send,
                    Signal::Stats(Stats {
                        generation,
                        position: core.double(c"time-pos").unwrap_or(0.),
                        duration: core.double(c"duration").unwrap_or(0.),
                        codec: core.string(c"video-format").unwrap_or_default(),
                        hwdec: core.string(c"hwdec-current").unwrap_or_default(),
                        dropped: core.int(c"frame-drop-count").unwrap_or(0),
                        decoder_dropped: core.int(c"decoder-frame-drop-count").unwrap_or(0),
                        eof: core.flag(c"eof-reached").unwrap_or(false),
                        paused: core.flag(c"pause"),
                        paused_for_cache: core.flag(c"paused-for-cache"),
                        muted: core.flag(c"mute").unwrap_or(true),
                    }),
                );
            }
        }
        let state = queue.state.lock().unwrap();
        if !state.shutdown
            && state.serial == serial
            && state.target.is_none()
            && state.muted.is_none()
            && state.seek.is_none()
            && state.paused.is_none()
        {
            let _ = queue
                .wake
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap();
        }
    }
    core.stop();
}
