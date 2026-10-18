use crate::config::{Config, PipelineConfig, TestAction};
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

struct PipelineRuntimeState {
    pipeline_id: String,
    virtual_port_name: String,
    target_port_name: String,
    rules: Vec<CompiledRule>,
    out_conn: Option<MidiOutputConnection>,
    logger: SharedLogger,
}

impl PipelineRuntimeState {
    fn ensure_target_open(&mut self) -> bool {
        if self.out_conn.is_some() {
            return true;
        }
        let Ok(midi_out) = MidiOutput::new("ReMIDI-Out") else {
            self.logger
                .error("Error: Could not initialize MIDI output subsystem");
            return false;
        };
        let ports = midi_out.ports();
        // Match exact name first, then substring match
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
                "[{}] Error: Could not find MIDI-out port named '{}'",
                self.pipeline_id, self.target_port_name
            ));
            return false;
        };

        let resolved_name = midi_out
            .port_name(port)
            .unwrap_or_else(|_| self.target_port_name.clone());

        match midi_out.connect(port, "ReMIDI-Out-Conn") {
            Ok(conn) => {
                self.logger.info(format!(
                    "[{}] Opened MIDI-out port '{}' (for sending)",
                    self.pipeline_id, resolved_name
                ));
                self.out_conn = Some(conn);
                true
            }
            Err(e) => {
                self.logger.error(format!(
                    "[{}] Error: Could not open MIDI-out port '{}': {}",
                    self.pipeline_id, resolved_name, e
                ));
                false
            }
        }
    }

    fn set_target_port(&mut self, new_target: String) {
        if self.target_port_name != new_target {
            if self.out_conn.take().is_some() {
                self.logger.info(format!(
                    "[{}] Closed MIDI-out port '{}'",
                    self.pipeline_id, self.target_port_name
                ));
            }
            self.target_port_name = new_target;
        }
        let _ = self.ensure_target_open();
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
                            "[{}] Error sending to MIDI-out port '{}': {}",
                            self.pipeline_id, self.target_port_name, e
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
                    "[{}] Error transforming message '{}': {}",
                    self.pipeline_id, in_hex, e
                ));
            }
        }
    }
}

pub struct ActivePipelineHandle {
    pub pipeline_index: usize,
    state: Arc<Mutex<PipelineRuntimeState>>,
    _backend: PipelineBackendHandle,
}

enum PipelineBackendHandle {
    #[allow(dead_code)]
    MidirInput(MidiInputConnection<()>),
    #[cfg(target_os = "windows")]
    #[allow(dead_code)]
    TeVirtualMidi(windows_virtual::TeVirtualPortGuard),
}

pub struct MidiEngine {
    active_pipelines: Vec<ActivePipelineHandle>,
    current_config: Option<Config>,
    logger: SharedLogger,
}

impl MidiEngine {
    pub fn new(logger: SharedLogger) -> Self {
        Self {
            active_pipelines: Vec::new(),
            current_config: None,
            logger,
        }
    }

    pub fn stop_all(&mut self) {
        self.active_pipelines.clear();
    }

    pub fn current_config(&self) -> Option<&Config> {
        self.current_config.as_ref()
    }

    pub fn load_config(&mut self, config: Config) -> Result<()> {
        self.stop_all();

        let mut any_error = false;
        for (idx, pipeline_cfg) in config.pipelines.iter().enumerate() {
            if !pipeline_cfg.enabled {
                self.logger.info(format!(
                    "[{}] Pipeline disabled (skipping virtual port '{}')",
                    pipeline_cfg.id, pipeline_cfg.virtual_port
                ));
                continue;
            }
            match start_pipeline(idx, pipeline_cfg, self.logger.clone()) {
                Ok(handle) => self.active_pipelines.push(handle),
                Err(e) => {
                    self.logger.error(format!("{:#}", e));
                    any_error = true;
                }
            }
        }
        self.current_config = Some(config);
        if !any_error {
            self.logger.info("listening...");
        }
        Ok(())
    }

    pub fn set_pipeline_target_port(&mut self, pipeline_idx: usize, new_target: String) {
        if let Some(cfg) = &mut self.current_config {
            if let Some(p) = cfg.pipelines.get_mut(pipeline_idx) {
                p.target_port = new_target.clone();
            }
        }
        if let Some(active) = self
            .active_pipelines
            .iter()
            .find(|a| a.pipeline_index == pipeline_idx)
        {
            if let Ok(mut guard) = active.state.lock() {
                guard.set_target_port(new_target);
            }
        }
    }

    pub fn run_pipeline_test(&self, pipeline_idx: usize) {
        let Some(cfg) = &self.current_config else {
            self.logger
                .error("Cannot run test: no configuration loaded.");
            return;
        };
        let Some(pipeline_cfg) = cfg.pipelines.get(pipeline_idx) else {
            self.logger
                .error(format!("Invalid pipeline index {}", pipeline_idx));
            return;
        };
        let Some(active) = self
            .active_pipelines
            .iter()
            .find(|a| a.pipeline_index == pipeline_idx)
        else {
            self.logger.error(format!(
                "[{}] Cannot run test: pipeline is currently disabled.",
                pipeline_cfg.id
            ));
            return;
        };

        let steps = match pipeline_cfg.parse_test_steps() {
            Ok(s) => s,
            Err(e) => {
                self.logger.error(format!(
                    "[{}] Invalid test configuration: {:#}",
                    pipeline_cfg.id, e
                ));
                return;
            }
        };

        if steps.is_empty() {
            self.logger.info(format!(
                "[{}] No test steps defined for this pipeline.",
                pipeline_cfg.id
            ));
            return;
        }

        let state = active.state.clone();
        let logger = self.logger.clone();
        let pipeline_id = pipeline_cfg.id.clone();

        thread::spawn(move || {
            logger.info(format!("[{}] executing tests...", pipeline_id));
            for step in steps {
                match step {
                    TestAction::Send(message) => match parse_midi_hex(&message, false) {
                        Ok(bytes) => {
                            if let Ok(mut guard) = state.lock() {
                                guard.handle_incoming_midi(&bytes);
                            }
                        }
                        Err(e) => {
                            logger.error(format!(
                                "[{}] Invalid test message '{}': {}",
                                pipeline_id, message, e
                            ));
                        }
                    },
                    TestAction::Sleep(ms) => {
                        thread::sleep(Duration::from_millis(ms));
                    }
                }
            }
            logger.info(format!("[{}] tests completed.", pipeline_id));
        });
    }
}

fn start_pipeline(
    pipeline_index: usize,
    pipeline_cfg: &PipelineConfig,
    logger: SharedLogger,
) -> Result<ActivePipelineHandle> {
    let mut compiled_rules = Vec::with_capacity(pipeline_cfg.rules.len());
    for r in &pipeline_cfg.rules {
        compiled_rules.push(CompiledRule::compile(r)?);
    }

    let mut runtime = PipelineRuntimeState {
        pipeline_id: pipeline_cfg.id.clone(),
        virtual_port_name: pipeline_cfg.virtual_port.clone(),
        target_port_name: pipeline_cfg.target_port.clone(),
        rules: compiled_rules,
        out_conn: None,
        logger: logger.clone(),
    };

    let _ = runtime.ensure_target_open();

    let state = Arc::new(Mutex::new(runtime));
    let backend = create_virtual_or_input_backend(
        &pipeline_cfg.id,
        &pipeline_cfg.virtual_port,
        state.clone(),
        &logger,
    )?;

    Ok(ActivePipelineHandle {
        pipeline_index,
        state,
        _backend: backend,
    })
}

#[cfg(unix)]
fn create_virtual_or_input_backend(
    pipeline_id: &str,
    port_name: &str,
    state: Arc<Mutex<PipelineRuntimeState>>,
    logger: &SharedLogger,
) -> Result<PipelineBackendHandle> {
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
        .map_err(|e| {
            anyhow!(
                "[{}] Error: Could not create virtual MIDI port '{}': {}",
                pipeline_id,
                port_name,
                e
            )
        })?;

    logger.info(format!(
        "[{}] Created MIDI-out port '{}' (for receiving)",
        pipeline_id, port_name
    ));
    Ok(PipelineBackendHandle::MidirInput(conn))
}

#[cfg(target_os = "windows")]
fn create_virtual_or_input_backend(
    pipeline_id: &str,
    port_name: &str,
    state: Arc<Mutex<PipelineRuntimeState>>,
    logger: &SharedLogger,
) -> Result<PipelineBackendHandle> {
    match windows_virtual::TeVirtualPortGuard::create(port_name, state.clone(), logger) {
        Ok(guard) => {
            logger.info(format!(
                "[{}] Created MIDI-out port '{}' (for receiving via teVirtualMIDI)",
                pipeline_id, port_name
            ));
            Ok(PipelineBackendHandle::TeVirtualMidi(guard))
        }
        Err(te_err) => Err(anyhow!(
            "[{}] Error: Could not create virtual MIDI-out port '{}': {}",
            pipeline_id,
            port_name,
            te_err
        )),
    }
}

#[cfg(target_os = "windows")]
mod windows_virtual {
    use super::{PipelineRuntimeState, SharedLogger};
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
        cb_ctx_ptr: *mut Arc<Mutex<PipelineRuntimeState>>,
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
        let state_arc = unsafe { &*(callback_instance as *const Arc<Mutex<PipelineRuntimeState>>) };
        let slice = unsafe { std::slice::from_raw_parts(midi_data_bytes, length as usize) };
        if let Ok(mut guard) = state_arc.lock() {
            guard.handle_incoming_midi(slice);
        }
    }

    impl TeVirtualPortGuard {
        pub fn create(
            port_name: &str,
            state: Arc<Mutex<PipelineRuntimeState>>,
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

                let wide_name: Vec<u16> =
                    port_name.encode_utf16().chain(std::iter::once(0)).collect();
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
