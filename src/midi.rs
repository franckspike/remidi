use crate::config::{Config, FilterConfig, NormalizedTestStep};
use crate::engine::{CompiledRule, RuleOutcome, format_midi_hex, parse_midi_hex, process_midi_bytes};
use anyhow::{Context, Result, anyhow};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Transform,
    Verbose,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Clone)]
pub struct SharedLogger {
    entries: Arc<Mutex<Vec<LogEntry>>>,
    has_unseen_error: Arc<AtomicBool>,
    pub verbose: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
}

impl SharedLogger {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
            has_unseen_error: Arc::new(AtomicBool::new(false)),
            verbose: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn log(&self, level: LogLevel, msg: impl Into<String>) {
        if level == LogLevel::Verbose && !self.verbose.load(Ordering::Relaxed) {
            return;
        }
        if level == LogLevel::Error {
            self.has_unseen_error.store(true, Ordering::SeqCst);
        }
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let secs = now_secs % 60;
        let mins = (now_secs / 60) % 60;
        let hours = (now_secs / 3600) % 24;
        let timestamp = format!("{:02}:{:02}:{:02}", hours, mins, secs);

        let entry = LogEntry {
            timestamp,
            level,
            message: msg.into(),
        };
        if let Ok(mut guard) = self.entries.lock() {
            guard.push(entry);
            if guard.len() > 2000 {
                let excess = guard.len() - 2000;
                guard.drain(0..excess);
            }
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.log(LogLevel::Info, msg);
    }

    pub fn transform(&self, msg: impl Into<String>) {
        self.log(LogLevel::Transform, msg);
    }

    pub fn verbose(&self, msg: impl Into<String>) {
        self.log(LogLevel::Verbose, msg);
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.log(LogLevel::Error, msg);
    }

    pub fn take_unseen_error(&self) -> bool {
        self.has_unseen_error.swap(false, Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }
}

pub fn list_midi_out_ports() -> Vec<String> {
    let Ok(midi_out) = MidiOutput::new("ReMIDI-Scanner") else {
        return Vec::new();
    };
    midi_out
        .ports()
        .iter()
        .filter_map(|p| midi_out.port_name(p).ok())
        .collect()
}

struct FilterRuntimeState {
    filter_id: String,
    virtual_port_name: String,
    target_port_name: String,
    rules: Vec<CompiledRule>,
    out_conn: Option<MidiOutputConnection>,
    logger: SharedLogger,
}

impl FilterRuntimeState {
    fn ensure_target_open(&mut self) -> bool {
        if self.out_conn.is_some() {
            return true;
        }
        let Ok(midi_out) = MidiOutput::new("ReMIDI-Out") else {
            self.logger.error("Error: Could not initialize MIDI output subsystem");
            return false;
        };
        let ports = midi_out.ports();
        // Match exact name first, then substring match (useful on Linux where ALSA appends `:0` or client ID)
        let matched_port = ports
            .iter()
            .find(|p| {
                midi_out
                    .port_name(p)
                    .map(|n| n == self.target_port_name)
                    .unwrap_or(false)
            })
            .or_else(|| {
                ports.iter().find(|p| {
                    midi_out
                        .port_name(p)
                        .map(|n| n.contains(&self.target_port_name))
                        .unwrap_or(false)
                })
            });

        let Some(port) = matched_port else {
            self.logger.error(format!(
                "Error: Could not find MIDI-out port named '{}'",
                self.target_port_name
            ));
            return false;
        };

        let resolved_name = midi_out
            .port_name(port)
            .unwrap_or_else(|_| self.target_port_name.clone());

        match midi_out.connect(port, "ReMIDI-Out-Conn") {
            Ok(conn) => {
                self.logger.info(format!(
                    "Opened MIDI-out port '{}' (for sending)",
                    resolved_name
                ));
                self.out_conn = Some(conn);
                true
            }
            Err(e) => {
                self.logger.error(format!(
                    "Error: Could not open MIDI-out port '{}': {}",
                    resolved_name, e
                ));
                false
            }
        }
    }

    fn handle_incoming_midi(&mut self, bytes: &[u8]) {
        if self.logger.paused.load(Ordering::Relaxed) {
            return;
        }
        if bytes.is_empty() {
            return;
        }
        let in_hex = format_midi_hex(bytes);
        match process_midi_bytes(&mut self.rules, bytes, Instant::now()) {
            Ok(RuleOutcome::Matched {
                output_bytes,
                comment,
                ..
            }) => {
                let out_hex = format_midi_hex(&output_bytes);
                if self.ensure_target_open() {
                    let send_res = if let Some(conn) = self.out_conn.as_mut() {
                        conn.send(&output_bytes)
                    } else {
                        Ok(())
                    };
                    if let Err(e) = send_res {
                        self.logger.error(format!(
                            "Error sending to MIDI-out port '{}': {}",
                            self.target_port_name, e
                        ));
                        self.out_conn = None;
                    } else {
                        let suffix = comment
                            .as_deref()
                            .map(|c| format!(" [{}]", c))
                            .unwrap_or_default();
                        self.logger.transform(format!(
                            "({}) {} --> {} ({}){}",
                            self.virtual_port_name, in_hex, out_hex, self.target_port_name, suffix
                        ));
                    }
                }
            }
            Ok(RuleOutcome::FilteredByDelay { .. }) => {
                self.logger.verbose(format!(
                    "{}: {} (filtered out by minimum delay)",
                    self.virtual_port_name, in_hex
                ));
            }
            Ok(RuleOutcome::NoMatch) => {
                self.logger.verbose(format!(
                    "{}: {} (no match, ignore)",
                    self.virtual_port_name, in_hex
                ));
            }
            Err(e) => {
                self.logger.error(format!(
                    "Error transforming message '{}' on '{}': {}",
                    in_hex, self.filter_id, e
                ));
            }
        }
    }
}

pub struct ActiveFilterHandle {
    state: Arc<Mutex<FilterRuntimeState>>,
    _backend: FilterBackendHandle,
}

enum FilterBackendHandle {
    #[allow(dead_code)]
    MidirInput(MidiInputConnection<()>),
    #[cfg(target_os = "windows")]
    #[allow(dead_code)]
    TeVirtualMidi(windows_virtual::TeVirtualPortGuard),
}

pub struct MidiEngine {
    active_filters: Vec<ActiveFilterHandle>,
    current_config: Option<Config>,
    logger: SharedLogger,
}

impl MidiEngine {
    pub fn new(logger: SharedLogger) -> Self {
        Self {
            active_filters: Vec::new(),
            current_config: None,
            logger,
        }
    }

    pub fn stop_all(&mut self) {
        self.active_filters.clear();
    }

    pub fn load_config(&mut self, config: Config) -> Result<()> {
        self.stop_all();

        for filter_cfg in &config.filters {
            let handle = start_filter(filter_cfg, self.logger.clone())?;
            self.active_filters.push(handle);
        }
        self.current_config = Some(config);
        self.logger.info("listening...");
        Ok(())
    }

    pub fn run_tests(&self) {
        let Some(cfg) = &self.current_config else {
            self.logger.error("Cannot run tests: no configuration loaded.");
            return;
        };
        if cfg.tests.is_empty() {
            self.logger.info("No test steps defined in current configuration.");
            return;
        }
        let steps: Vec<NormalizedTestStep> = cfg
            .tests
            .iter()
            .filter_map(|s| s.normalized())
            .collect();
        let states: Vec<Arc<Mutex<FilterRuntimeState>>> = self
            .active_filters
            .iter()
            .map(|f| f.state.clone())
            .collect();
        let logger = self.logger.clone();

        thread::spawn(move || {
            logger.info("executing tests...");
            for step in steps {
                match step {
                    NormalizedTestStep::Send {
                        message,
                        dec_second_byte,
                    } => match parse_midi_hex(&message, dec_second_byte) {
                        Ok(bytes) => {
                            for st in &states {
                                if let Ok(mut guard) = st.lock() {
                                    guard.handle_incoming_midi(&bytes);
                                }
                            }
                        }
                        Err(e) => {
                            logger.error(format!("Invalid test message '{}': {}", message, e));
                        }
                    },
                    NormalizedTestStep::Sleep { duration_ms } => {
                        thread::sleep(Duration::from_millis(duration_ms));
                    }
                }
            }
            logger.info("tests completed.");
        });
    }
}

fn start_filter(filter_cfg: &FilterConfig, logger: SharedLogger) -> Result<ActiveFilterHandle> {
    let mut compiled_rules = Vec::with_capacity(filter_cfg.rules.len());
    for r in &filter_cfg.rules {
        compiled_rules.push(CompiledRule::compile(r)?);
    }

    let mut runtime = FilterRuntimeState {
        filter_id: filter_cfg.id.clone(),
        virtual_port_name: filter_cfg.virtual_port.clone(),
        target_port_name: filter_cfg.target_port.clone(),
        rules: compiled_rules,
        out_conn: None,
        logger: logger.clone(),
    };

    // Try opening target port right away (logs warning/error if not yet plugged in, but will also retry on message)
    let _ = runtime.ensure_target_open();

    let state = Arc::new(Mutex::new(runtime));
    let backend = create_virtual_or_input_backend(&filter_cfg.virtual_port, state.clone(), &logger)?;

    Ok(ActiveFilterHandle {
        state,
        _backend: backend,
    })
}

#[cfg(unix)]
fn create_virtual_or_input_backend(
    port_name: &str,
    state: Arc<Mutex<FilterRuntimeState>>,
    logger: &SharedLogger,
) -> Result<FilterBackendHandle> {
    use midir::os::unix::VirtualInput;
    let midi_in = MidiInput::new("ReMIDI").context("Failed to initialize ALSA/JACK MidiInput")?;
    let state_cb = state.clone();
    let conn = midi_in
        .create_virtual(
            port_name,
            move |_stamp, message, _| {
                if let Ok(mut guard) = state_cb.lock() {
                    guard.handle_incoming_midi(message);
                }
            },
            (),
        )
        .map_err(|e| anyhow!("Error: Could not create virtual MIDI port '{}': {}", port_name, e))?;

    logger.info(format!(
        "Created MIDI-out port '{}' (for receiving)",
        port_name
    ));
    Ok(FilterBackendHandle::MidirInput(conn))
}

#[cfg(target_os = "windows")]
fn create_virtual_or_input_backend(
    port_name: &str,
    state: Arc<Mutex<FilterRuntimeState>>,
    logger: &SharedLogger,
) -> Result<FilterBackendHandle> {
    // 1. Try teVirtualMIDI64.dll first (just like original ReMIDI.exe)
    match windows_virtual::TeVirtualPortGuard::create(port_name, state.clone(), logger) {
        Ok(guard) => {
            logger.info(format!(
                "Created MIDI-out port '{}' (for receiving via teVirtualMIDI)",
                port_name
            ));
            return Ok(FilterBackendHandle::TeVirtualMidi(guard));
        }
        Err(te_err) => {
            logger.verbose(format!(
                "teVirtualMIDI unavailable ({}), checking existing MIDI-in ports...",
                te_err
            ));
        }
    }

    // 2. Fallback: open an existing MIDI-in port (e.g., loopMIDI port with matching name)
    let midi_in = MidiInput::new("ReMIDI").context("Failed to initialize Windows MidiInput")?;
    let ports = midi_in.ports();
    let matched = ports.iter().find(|p| {
        midi_in
            .port_name(p)
            .map(|n| n == port_name || n.contains(port_name))
            .unwrap_or(false)
    });

    if let Some(port) = matched {
        let state_cb = state;
        let conn = midi_in
            .connect(
                port,
                "ReMIDI-In-Conn",
                move |_stamp, message, _| {
                    if let Ok(mut guard) = state_cb.lock() {
                        guard.handle_incoming_midi(message);
                    }
                },
                (),
            )
            .map_err(|e| anyhow!("Failed to open MIDI-in port '{}': {}", port_name, e))?;
        logger.info(format!(
            "Opened existing MIDI-in port '{}' (for receiving)",
            port_name
        ));
        Ok(FilterBackendHandle::MidirInput(conn))
    } else {
        bail!(
            "Error: Could not create virtual MIDI port '{}' (teVirtualMIDI64.dll not found and no existing MIDI-in port named '{}')",
            port_name,
            port_name
        );
    }
}

#[cfg(target_os = "windows")]
mod windows_virtual {
    use super::{FilterRuntimeState, SharedLogger};
    use anyhow::{Context, Result, bail};
    use libloading::{Library, Symbol};
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    type LPVM_MIDI_PORT = *mut c_void;
    type TeVmCallback = unsafe extern "system" fn(
        midi_port: LPVM_MIDI_PORT,
        midi_data_bytes: *const u8,
        length: u32,
         callback_instance: usize,
    );

    type FnCreatePortEx2 = unsafe extern "system" fn(
        port_name: *const u16,
        callback: TeVmCallback,
        dw_callback_instance: usize,
        max_sysex_length: u32,
        flags: u32,
    ) -> LPVM_MIDI_PORT;

    type FnClosePort = unsafe extern "system" fn(midi_port: LPVM_MIDI_PORT);

    const TE_VM_FLAGS_PARSE_RX: u32 = 1;

    pub struct TeVirtualPortGuard {
        _lib: Library,
        port: LPVM_MIDI_PORT,
        close_fn: FnClosePort,
        cb_ctx_ptr: *mut Arc<Mutex<FilterRuntimeState>>,
    }

    unsafe impl Send for TeVirtualPortGuard {}
    unsafe impl Sync for TeVirtualPortGuard {}

    unsafe extern "system" fn te_vm_callback(
        _port: LPVM_MIDI_PORT,
        midi_data_bytes: *const u8,
        length: u32,
        callback_instance: usize,
    ) {
        if midi_data_bytes.is_null() || length == 0 || callback_instance == 0 {
            return;
        }
        let state_arc = unsafe { &*(callback_instance as *const Arc<Mutex<FilterRuntimeState>>) };
        let slice = unsafe { std::slice::from_raw_parts(midi_data_bytes, length as usize) };
        if let Ok(mut guard) = state_arc.lock() {
            guard.handle_incoming_midi(slice);
        }
    }

    impl TeVirtualPortGuard {
        pub fn create(
            port_name: &str,
            state: Arc<Mutex<FilterRuntimeState>>,
            _logger: &SharedLogger,
        ) -> Result<Self> {
            unsafe {
                let lib = Library::new("teVirtualMIDI64.dll")
                    .or_else(|_| Library::new("teVirtualMIDI.dll"))
                    .context("Could not load teVirtualMIDI64.dll")?;

                let create_fn: Symbol<FnCreatePortEx2> = lib
                    .get(b"virtualMIDICreatePortEx2\0")
                    .context("Missing virtualMIDICreatePortEx2")?;
                let close_sym: Symbol<FnClosePort> = lib
                    .get(b"virtualMIDIClosePort\0")
                    .context("Missing virtualMIDIClosePort")?;
                let close_fn = *close_sym;

                let wide_name: Vec<u16> = port_name.encode_utf16().chain(std::iter::once(0)).collect();
                let boxed_state = Box::new(state);
                let cb_ctx_ptr = Box::into_raw(boxed_state);

                let port = create_fn(
                    wide_name.as_ptr(),
                    te_vm_callback,
                    cb_ctx_ptr as usize,
                    0,
                    TE_VM_FLAGS_PARSE_RX,
                );

                if port.is_null() {
                    let _ = Box::from_raw(cb_ctx_ptr);
                    bail!("virtualMIDICreatePortEx2 returned NULL");
                }

                Ok(Self {
                    _lib: lib,
                    port,
                    close_fn,
                    cb_ctx_ptr,
                })
            }
        }
    }

    impl Drop for TeVirtualPortGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.port.is_null() {
                    (self.close_fn)(self.port);
                }
                if !self.cb_ctx_ptr.is_null() {
                    let _ = Box::from_raw(self.cb_ctx_ptr);
                }
            }
        }
    }
}
