use crate::config::{Config, PipelineConfig, TestAction};
use crate::engine::{CompiledRule, RuleOutcome, format_midi_hex, parse_midi_hex, process_midi_bytes};
#[allow(unused_imports)]
use anyhow::{Context, Result, anyhow};
#[allow(unused_imports)]
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use std::collections::HashMap;
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
    #[cfg(target_os = "linux")]
    {
        crate::alsa_backend::AlsaClient::list_available_ports()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(midi_out) = MidiOutput::new("ReMIDI-Scanner") else {
            return Vec::new();
        };
        midi_out
            .ports()
            .iter()
            .filter_map(|p| midi_out.port_name(p).ok())
            .collect()
    }
}

pub enum OutputConnection {
    #[cfg(target_os = "linux")]
    Alsa(crate::alsa_backend::AlsaOutputSender),
    #[cfg(not(target_os = "linux"))]
    Midir(MidiOutputConnection),
}

impl OutputConnection {
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            OutputConnection::Alsa(sender) => sender.send(bytes),
            #[cfg(not(target_os = "linux"))]
            OutputConnection::Midir(conn) => {
                conn.send(bytes).map_err(|e| anyhow!("{:#}", e))
            }
        }
    }
}

/// Shared pool of open MIDI-out connections.
/// Ensures that if multiple pipelines target the same hardware MIDI-out port:
/// 1. The OS MIDI-out port is opened only once (preventing Windows MMSYSERR_ALLOCATED errors).
/// 2. Concurrent `send(&bytes)` calls across pipelines are strictly serialized via a Mutex so MIDI packets never interleave.
#[derive(Clone)]
pub struct SharedOutputPool {
    connections: Arc<Mutex<HashMap<String, Arc<Mutex<OutputConnection>>>>>,
    #[cfg(target_os = "linux")]
    alsa_client: Option<Arc<crate::alsa_backend::AlsaClient>>,
    logger: SharedLogger,
}

impl SharedOutputPool {
    pub fn new(
        logger: SharedLogger,
        #[cfg(target_os = "linux")] alsa_client: Option<Arc<crate::alsa_backend::AlsaClient>>,
    ) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(target_os = "linux")]
            alsa_client,
            logger,
        }
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.connections.lock() {
            map.clear();
        }
    }

    /// Opens (or reuses an already open) MIDI-out connection for `target_port_name`.
    pub fn ensure_open(
        &self,
        pipeline_id: &str,
        target_port_name: &str,
    ) -> Option<Arc<Mutex<OutputConnection>>> {
        let mut map = self.connections.lock().ok()?;

        #[cfg(target_os = "linux")]
        {
            if let Some(existing) = map.get(pipeline_id)
                && let Ok(guard) = existing.lock()
            {
                let OutputConnection::Alsa(ref sender) = *guard;
                if sender.resolved_name == target_port_name
                    || target_port_name.is_empty()
                    || sender.resolved_name.contains(target_port_name)
                {
                    return Some(existing.clone());
                }
            }

            let Some(client) = &self.alsa_client else {
                self.logger.error("ALSA client not initialized");
                return None;
            };
            match client.open_output_port(pipeline_id, target_port_name) {
                Ok(sender) => {
                    let arc = Arc::new(Mutex::new(OutputConnection::Alsa(sender)));
                    map.insert(pipeline_id.to_string(), arc.clone());
                    Some(arc)
                }
                Err(e) => {
                    self.logger.error(format!("{:#}", e));
                    None
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            if let Some(existing) = map.get(target_port_name) {
                return Some(existing.clone());
            }

            let Ok(midi_out) = MidiOutput::new("ReMIDI-Out") else {
                self.logger
                    .error("Error: Could not initialize MIDI output subsystem");
                return None;
            };
            let ports = midi_out.ports();
            // Match exact name first, then substring match
            let matched_port = ports
                .iter()
                .find(|p| {
                    midi_out
                        .port_name(p)
                        .map(|n| n == target_port_name)
                        .unwrap_or(false)
                })
                .or_else(|| {
                    ports.iter().find(|p| {
                        midi_out
                            .port_name(p)
                            .map(|n| n.contains(target_port_name))
                            .unwrap_or(false)
                    })
                });

            let Some(port) = matched_port else {
                self.logger.error(format!(
                    "[{}] Error: Could not find MIDI-out port named '{}'",
                    pipeline_id, target_port_name
                ));
                return None;
            };

            let resolved_name = midi_out
                .port_name(port)
                .unwrap_or_else(|_| target_port_name.to_string());

            // Check if another pipeline already opened the same resolved port under a substring alias
            if let Some(existing) = map.get(&resolved_name) {
                let arc = existing.clone();
                map.insert(target_port_name.to_string(), arc.clone());
                return Some(arc);
            }

            match midi_out.connect(port, "ReMIDI-Out-Conn") {
                Ok(conn) => {
                    self.logger.info(format!(
                        "[{}] Opened MIDI-out port '{}' (for sending)",
                        pipeline_id, resolved_name
                    ));
                    let arc = Arc::new(Mutex::new(OutputConnection::Midir(conn)));
                    map.insert(target_port_name.to_string(), arc.clone());
                    if resolved_name != target_port_name {
                        map.insert(resolved_name, arc.clone());
                    }
                    Some(arc)
                }
                Err(e) => {
                    self.logger.error(format!(
                        "[{}] Error: Could not open MIDI-out port '{}': {}",
                        pipeline_id, resolved_name, e
                    ));
                    None
                }
            }
        }
    }

    pub fn send_synchronized(&self, pipeline_id: &str, target_port_name: &str, bytes: &[u8]) -> bool {
        let Some(conn_arc) = self.ensure_open(pipeline_id, target_port_name) else {
            return false;
        };
        let send_res = if let Ok(mut conn) = conn_arc.lock() {
            conn.send(bytes)
        } else {
            return false;
        };

        if let Err(e) = send_res {
            self.logger.error(format!(
                "[{}] Error sending to MIDI-out port '{}': {:#}",
                pipeline_id, target_port_name, e
            ));
            if let Ok(mut map) = self.connections.lock() {
                map.remove(target_port_name);
            }
            false
        } else {
            true
        }
    }
}

pub(crate) struct PipelineRuntimeState {
    pipeline_id: String,
    full_virtual_port_name: String,
    target_port_name: String,
    rules: Vec<CompiledRule>,
    output_pool: SharedOutputPool,
    logger: SharedLogger,
}

impl PipelineRuntimeState {
    fn set_target_port(&mut self, new_target: String) {
        self.target_port_name = new_target.clone();
        let _ = self.output_pool.ensure_open(&self.pipeline_id, &new_target);
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
                if self
                    .output_pool
                    .send_synchronized(&self.pipeline_id, &self.target_port_name, &output_bytes)
                {
                    self.logger.verbose(format!(
                        "[{}] {} --> {}",
                        self.pipeline_id,
                        in_hex,
                        out_hex,
                    ));
                    let suffix = comment
                        .as_deref()
                        .map(|c| format!("{}", c))
                        .unwrap_or_default();
                    self.logger.transform(format!(
                        "[{}] {}",
                        self.pipeline_id,
                        suffix
                    ));
                }
            }
            Ok(RuleOutcome::FilteredByDebounce { .. }) => {
                self.logger.verbose(format!(
                    "[{}] {}: {} (filtered out by debounce)",
                    self.pipeline_id, self.full_virtual_port_name, in_hex
                ));
            }
            Ok(RuleOutcome::NoMatch) => {
                self.logger.verbose(format!(
                    "[{}] {}: {} (no match, ignore)",
                    self.pipeline_id, self.full_virtual_port_name, in_hex
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
}

struct SharedVirtualPort {
    subscribers: Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>,
    _backend: VirtualBackendHandle,
}

enum VirtualBackendHandle {
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    Alsa(crate::alsa_backend::AlsaVirtualPortHandle),
    #[cfg(all(unix, not(target_os = "linux")))]
    MidirInput(MidiInputConnection<()>),
    #[cfg(target_os = "windows")]
    TeVirtualMidi(windows_virtual::TeVirtualPortGuard),
}

pub struct MidiEngine {
    active_pipelines: Vec<ActivePipelineHandle>,
    virtual_ports: HashMap<String, SharedVirtualPort>,
    output_pool: SharedOutputPool,
    #[cfg(target_os = "linux")]
    alsa_client: Option<Arc<crate::alsa_backend::AlsaClient>>,
    current_config: Option<Config>,
    logger: SharedLogger,
}

impl MidiEngine {
    pub fn new(logger: SharedLogger) -> Self {
        #[cfg(target_os = "linux")]
        let alsa_client = match crate::alsa_backend::AlsaClient::new(logger.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                logger.error(format!("Failed to initialize unified ALSA device: {:#}", e));
                None
            }
        };

        let output_pool = SharedOutputPool::new(
            logger.clone(),
            #[cfg(target_os = "linux")]
            alsa_client.clone(),
        );

        Self {
            active_pipelines: Vec::new(),
            virtual_ports: HashMap::new(),
            output_pool,
            #[cfg(target_os = "linux")]
            alsa_client,
            current_config: None,
            logger,
        }
    }

    pub fn stop_all(&mut self) {
        self.active_pipelines.clear();
        self.virtual_ports.clear();
        self.output_pool.clear();
    }

    pub fn current_config(&self) -> Option<&Config> {
        self.current_config.as_ref()
    }

    pub fn load_config(&mut self, config: Config) -> Result<()> {
        self.stop_all();

        let mut any_error = false;
        for (idx, pipeline_cfg) in config.pipelines.iter().enumerate() {
            let full_vport = pipeline_cfg.full_virtual_port_name();
            if !pipeline_cfg.enabled {
                self.logger.info(format!(
                    "[{}] Pipeline disabled (skipping '{}')",
                    pipeline_cfg.id, full_vport
                ));
                continue;
            }

            match self.register_pipeline(idx, pipeline_cfg) {
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

    fn register_pipeline(
        &mut self,
        pipeline_index: usize,
        pipeline_cfg: &PipelineConfig,
    ) -> Result<ActivePipelineHandle> {
        let mut compiled_rules = Vec::with_capacity(pipeline_cfg.rules.len());
        for r in &pipeline_cfg.rules {
            compiled_rules.push(CompiledRule::compile(r)?);
        }

        let full_virtual_port_name = pipeline_cfg.full_virtual_port_name();

        // Eagerly open (or share) the target MIDI-out port
        let _ = self
            .output_pool
            .ensure_open(&pipeline_cfg.id, &pipeline_cfg.target_port);

        let runtime = PipelineRuntimeState {
            pipeline_id: pipeline_cfg.id.clone(),
            full_virtual_port_name: full_virtual_port_name.clone(),
            target_port_name: pipeline_cfg.target_port.clone(),
            rules: compiled_rules,
            output_pool: self.output_pool.clone(),
            logger: self.logger.clone(),
        };

        let state = Arc::new(Mutex::new(runtime));

        // If a virtual port with this name already exists, subscribe this pipeline to it!
        if let Some(existing_vport) = self.virtual_ports.get(&full_virtual_port_name) {
            if let Ok(mut subs) = existing_vport.subscribers.lock() {
                subs.push(state.clone());
            }
            self.logger.info(format!(
                "[{}] Subscribed to shared MIDI-out port '{}' (for receiving)",
                pipeline_cfg.id, full_virtual_port_name
            ));
        } else {
            let subscribers = Arc::new(Mutex::new(vec![state.clone()]));
            let backend = create_virtual_backend(
                #[cfg(target_os = "linux")]
                self.alsa_client.as_ref().ok_or_else(|| anyhow!("ALSA client not available"))?,
                &pipeline_cfg.id,
                &full_virtual_port_name,
                subscribers.clone(),
                &self.logger,
            )?;
            self.virtual_ports.insert(
                full_virtual_port_name,
                SharedVirtualPort {
                    subscribers,
                    _backend: backend,
                },
            );
        }

        Ok(ActivePipelineHandle {
            pipeline_index,
            state,
        })
    }

    pub fn set_pipeline_target_port(&mut self, pipeline_idx: usize, new_target: String) {
        if let Some(cfg) = &mut self.current_config
            && let Some(p) = cfg.pipelines.get_mut(pipeline_idx)
        {
            p.target_port = new_target.clone();
        }
        if let Some(active) = self
            .active_pipelines
            .iter()
            .find(|a| a.pipeline_index == pipeline_idx)
            && let Ok(mut guard) = active.state.lock()
        {
            guard.set_target_port(new_target);
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

    pub fn run_all_pipeline_tests(&self) {
        let Some(cfg) = &self.current_config else {
            self.logger
                .error("Cannot run tests: no configuration loaded.");
            return;
        };
        let mut to_run = Vec::new();
        for (idx, pipeline_cfg) in cfg.pipelines.iter().enumerate() {
            if !pipeline_cfg.enabled || pipeline_cfg.test.is_empty() {
                continue;
            }
            let Some(active) = self
                .active_pipelines
                .iter()
                .find(|a| a.pipeline_index == idx)
            else {
                continue;
            };
            match pipeline_cfg.parse_test_steps() {
                Ok(steps) if !steps.is_empty() => {
                    to_run.push((pipeline_cfg.id.clone(), active.state.clone(), steps));
                }
                Ok(_) => {}
                Err(e) => {
                    self.logger.error(format!(
                        "[{}] Invalid test configuration: {:#}",
                        pipeline_cfg.id, e
                    ));
                }
            }
        }

        if to_run.is_empty() {
            self.logger
                .info("No enabled pipelines have test steps defined.");
            return;
        }

        let logger = self.logger.clone();
        thread::spawn(move || {
            logger.info("Executing tests for all enabled pipelines...");
            for (pipeline_id, state, steps) in to_run {
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
            }
            logger.info("All pipeline tests completed.");
        });
    }
}

pub(crate) fn dispatch_to_subscribers(
    subscribers: &Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>,
    message: &[u8],
) {
    let list = if let Ok(guard) = subscribers.lock() {
        guard.clone()
    } else {
        return;
    };
    for sub in list {
        if let Ok(mut state) = sub.lock() {
            state.handle_incoming_midi(message);
        }
    }
}

#[cfg(target_os = "linux")]
fn create_virtual_backend(
    alsa_client: &Arc<crate::alsa_backend::AlsaClient>,
    pipeline_id: &str,
    port_name: &str,
    subscribers: Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>,
    logger: &SharedLogger,
) -> Result<VirtualBackendHandle> {
    let handle = alsa_client.create_virtual_port(port_name, subscribers)?;
    logger.info(format!(
        "[{}] Created MIDI-out port '{}' (for receiving)",
        pipeline_id, port_name
    ));
    Ok(VirtualBackendHandle::Alsa(handle))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn create_virtual_backend(
    pipeline_id: &str,
    port_name: &str,
    subscribers: Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>,
    logger: &SharedLogger,
) -> Result<VirtualBackendHandle> {
    use midir::os::unix::VirtualInput;
    let midi_in = MidiInput::new("ReMIDI").context("Failed to initialize ALSA/JACK MidiInput")?;
    let subs_cb = subscribers;
    let conn = midi_in
        .create_virtual(
            port_name,
            move |_stamp, message, _| {
                dispatch_to_subscribers(&subs_cb, message);
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
    Ok(VirtualBackendHandle::MidirInput(conn))
}

#[cfg(target_os = "windows")]
fn create_virtual_backend(
    pipeline_id: &str,
    port_name: &str,
    subscribers: Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>,
    logger: &SharedLogger,
) -> Result<VirtualBackendHandle> {
    match windows_virtual::TeVirtualPortGuard::create(port_name, subscribers, logger) {
        Ok(guard) => {
            logger.info(format!(
                "[{}] Created MIDI-out port '{}' (for receiving via teVirtualMIDI)",
                pipeline_id, port_name
            ));
            Ok(VirtualBackendHandle::TeVirtualMidi(guard))
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
    use super::{PipelineRuntimeState, SharedLogger, dispatch_to_subscribers};
    use anyhow::{Context, Result, bail};
    use libloading::{Library, Symbol};
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    type LPVM_MIDI_PORT = *mut c_void;
    type SubscribersList = Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>;

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
        cb_ctx_ptr: *mut SubscribersList,
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
        let subscribers = unsafe { &*(callback_instance as *const SubscribersList) };
        let slice = unsafe { std::slice::from_raw_parts(midi_data_bytes, length as usize) };
        dispatch_to_subscribers(subscribers, slice);
    }

    impl TeVirtualPortGuard {
        pub fn create(
            port_name: &str,
            subscribers: SubscribersList,
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
                let boxed_subs = Box::new(subscribers);
                let cb_ctx_ptr = Box::into_raw(boxed_subs);

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
