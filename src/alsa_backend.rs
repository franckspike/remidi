use crate::midi::{PipelineRuntimeState, SharedLogger, dispatch_to_subscribers};
use alsa::poll::Descriptors;
use alsa::seq::{
    Addr, ClientIter, EventType, MidiEvent, PortCap, PortIter, PortSubscribe, PortType, Seq,
};
use alsa::Direction;
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

pub type SubscribersList = Arc<Mutex<Vec<Arc<Mutex<PipelineRuntimeState>>>>>;

pub struct AlsaClient {
    pub seq: Arc<Mutex<Seq>>,
    pub client_id: i32,
    pub ports: Arc<Mutex<HashMap<i32, SubscribersList>>>,
    running: Arc<AtomicBool>,
    thread_handle: Option<JoinHandle<()>>,
    pipe_tx: i32,
    logger: SharedLogger,
}

impl AlsaClient {
    pub fn new(logger: SharedLogger) -> Result<Arc<Self>> {
        let seq = Seq::open(None, None, true).context("Failed to open ALSA Sequencer duplex")?;
        let c_name = CString::new("ReMIDI").unwrap();
        seq.set_client_name(&c_name)
            .context("Failed to set ALSA client name")?;
        let client_id = seq.client_id().context("Failed to get ALSA client ID")?;

        let mut fds = [0i32; 2];
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(anyhow!("Failed to create pipe for ALSA event handler"));
        }
        let pipe_rx = fds[0];
        let pipe_tx = fds[1];

        let seq_arc = Arc::new(Mutex::new(seq));
        let ports: Arc<Mutex<HashMap<i32, SubscribersList>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let running = Arc::new(AtomicBool::new(true));

        let thread_seq = seq_arc.clone();
        let thread_ports = ports.clone();
        let thread_running = running.clone();

        let thread_handle = thread::spawn(move || {
            run_alsa_event_loop(
                thread_seq,
                thread_ports,
                thread_running,
                pipe_rx,
            );
            unsafe {
                libc::close(pipe_rx);
            }
        });

        Ok(Arc::new(Self {
            seq: seq_arc,
            client_id,
            ports,
            running,
            thread_handle: Some(thread_handle),
            pipe_tx,
            logger,
        }))
    }

    pub fn create_virtual_port(
        self: &Arc<Self>,
        port_name: &str,
        subscribers: SubscribersList,
    ) -> Result<AlsaVirtualPortHandle> {
        let seq = self.seq.lock().map_err(|_| anyhow!("Poisoned Seq mutex"))?;
        let c_name = CString::new(port_name)
            .map_err(|_| anyhow!("Port name contains null bytes"))?;

        let port_id = seq
            .create_simple_port(
                &c_name,
                PortCap::WRITE | PortCap::SUBS_WRITE,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .map_err(|e| anyhow!("Failed to create ALSA virtual port '{}': {}", port_name, e))?;

        drop(seq);

        if let Ok(mut map) = self.ports.lock() {
            map.insert(port_id, subscribers);
        }

        Ok(AlsaVirtualPortHandle {
            client: self.clone(),
            port_id,
            name: port_name.to_string(),
        })
    }

    pub fn open_output_port(
        self: &Arc<Self>,
        pipeline_id: &str,
        target_port_name: &str,
    ) -> Result<AlsaOutputSender> {
        let seq = self.seq.lock().map_err(|_| anyhow!("Poisoned Seq mutex"))?;

        let mut matched: Option<(Addr, String)> = None;
        for c in ClientIter::new(&seq) {
            if c.get_client() == self.client_id {
                continue;
            }
            for p in PortIter::new(&seq, c.get_client()) {
                let ptype = p.get_type();
                if !ptype.intersects(
                    PortType::MIDI_GENERIC | PortType::SYNTH | PortType::APPLICATION,
                ) {
                    continue;
                }
                let caps = p.get_capability();
                if !caps.contains(PortCap::WRITE | PortCap::SUBS_WRITE) {
                    continue;
                }

                let cname = c.get_name().unwrap_or_default();
                let pname = p.get_name().unwrap_or_default();
                let full_name =
                    format!("{}:{} {}:{}", cname, pname, p.get_client(), p.get_port());
                let addr_str = format!("{}:{}", p.get_client(), p.get_port());

                if full_name == target_port_name
                    || pname == target_port_name
                    || addr_str == target_port_name
                {
                    matched = Some((p.addr(), full_name));
                    break;
                }
                if (full_name.contains(target_port_name) || pname.contains(target_port_name))
                    && matched.is_none()
                {
                    matched = Some((p.addr(), full_name));
                }
            }
            if let Some((_, ref name)) = matched
                && name == target_port_name
            {
                break;
            }
        }

        let Some((dest_addr, resolved_name)) = matched else {
            return Err(anyhow!(
                "[{}] Error: Could not find MIDI-out port named '{}'",
                pipeline_id,
                target_port_name
            ));
        };

        let sender_port_name = format!("ReMIDI-Out [{}]", resolved_name);
        let c_sender_name = CString::new(sender_port_name.as_str())
            .unwrap_or_else(|_| CString::new("ReMIDI-Out").unwrap());

        let sender_port = seq
            .create_simple_port(
                &c_sender_name,
                PortCap::READ | PortCap::SUBS_READ,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .map_err(|e| anyhow!("Failed to create ALSA sender port: {}", e))?;

        let sub = PortSubscribe::empty()
            .map_err(|e| anyhow!("Failed to allocate PortSubscribe: {}", e))?;
        sub.set_sender(Addr {
            client: self.client_id,
            port: sender_port,
        });
        sub.set_dest(dest_addr);
        sub.set_time_update(true);
        sub.set_time_real(true);

        if let Err(e) = seq.subscribe_port(&sub) {
            let _ = seq.delete_port(sender_port);
            return Err(anyhow!(
                "Failed to connect ReMIDI output port to '{}': {}",
                resolved_name,
                e
            ));
        }

        drop(seq);

        let coder = MidiEvent::new(1024)
            .map_err(|e| anyhow!("Failed to create ALSA MidiEvent encoder: {}", e))?;
        coder.enable_running_status(false);

        self.logger.info(format!(
            "[{}] Opened MIDI-out port '{}' (for sending)",
            pipeline_id, resolved_name
        ));

        Ok(AlsaOutputSender {
            seq: self.seq.clone(),
            sender_port,
            sub,
            coder,
            resolved_name,
        })
    }

    #[allow(dead_code)]
    pub fn list_output_ports(&self) -> Vec<String> {
        let Ok(seq) = self.seq.lock() else {
            return Vec::new();
        };
        Self::list_ports_from_seq(&seq, self.client_id)
    }

    pub fn list_available_ports() -> Vec<String> {
        let Ok(seq) = Seq::open(None, Some(Direction::Playback), true) else {
            return Vec::new();
        };
        Self::list_ports_from_seq(&seq, -1)
    }

    fn list_ports_from_seq(seq: &Seq, ignore_client: i32) -> Vec<String> {
        let mut results = Vec::new();
        for c in ClientIter::new(seq) {
            if c.get_client() == ignore_client {
                continue;
            }
            if let Ok(cname) = c.get_name()
                && cname == "ReMIDI"
            {
                continue;
            }
            for p in PortIter::new(seq, c.get_client()) {
                let ptype = p.get_type();
                if !ptype.intersects(
                    PortType::MIDI_GENERIC | PortType::SYNTH | PortType::APPLICATION,
                ) {
                    continue;
                }
                let caps = p.get_capability();
                if !caps.contains(PortCap::WRITE | PortCap::SUBS_WRITE) {
                    continue;
                }
                let cname = c.get_name().unwrap_or_default();
                let pname = p.get_name().unwrap_or_default();
                let formatted =
                    format!("{}:{} {}:{}", cname, pname, p.get_client(), p.get_port());
                results.push(formatted);
            }
        }
        results
    }
}

impl Drop for AlsaClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let b = [1u8];
        unsafe {
            libc::write(self.pipe_tx, b.as_ptr() as *const libc::c_void, 1);
            libc::close(self.pipe_tx);
        }
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

pub struct AlsaVirtualPortHandle {
    client: Arc<AlsaClient>,
    pub port_id: i32,
    #[allow(dead_code)]
    pub name: String,
}

impl Drop for AlsaVirtualPortHandle {
    fn drop(&mut self) {
        if let Ok(mut ports) = self.client.ports.lock() {
            ports.remove(&self.port_id);
        }
        if let Ok(seq) = self.client.seq.lock() {
            let _ = seq.delete_port(self.port_id);
        }
    }
}

pub struct AlsaOutputSender {
    seq: Arc<Mutex<Seq>>,
    sender_port: i32,
    sub: PortSubscribe,
    coder: MidiEvent,
    pub resolved_name: String,
}

unsafe impl Send for AlsaOutputSender {}
unsafe impl Sync for AlsaOutputSender {}

impl AlsaOutputSender {
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Ok(seq) = self.seq.lock() else {
            return Err(anyhow!("Poisoned Seq mutex"));
        };

        let mut remaining = bytes;
        while !remaining.is_empty() {
            let (consumed, opt_ev) = self
                .coder
                .encode(remaining)
                .map_err(|e| anyhow!("ALSA encode error: {}", e))?;

            if consumed == 0 {
                break;
            }

            if let Some(mut ev) = opt_ev {
                ev.set_source(self.sender_port);
                ev.set_subs();
                ev.set_direct();
                seq.event_output_direct(&mut ev)
                    .map_err(|e| anyhow!("ALSA output error: {}", e))?;
            }
            remaining = &remaining[consumed..];
        }
        let _ = seq.drain_output();
        Ok(())
    }
}

impl Drop for AlsaOutputSender {
    fn drop(&mut self) {
        if let Ok(seq) = self.seq.lock() {
            let _ = seq.unsubscribe_port(self.sub.get_sender(), self.sub.get_dest());
            let _ = seq.delete_port(self.sender_port);
        }
    }
}

fn run_alsa_event_loop(
    seq_arc: Arc<Mutex<Seq>>,
    ports_arc: Arc<Mutex<HashMap<i32, SubscribersList>>>,
    running: Arc<AtomicBool>,
    pipe_rx: i32,
) {
    let mut poll_fds = vec![libc::pollfd {
        fd: pipe_rx,
        events: libc::POLLIN,
        revents: 0,
    }];

    {
        let Ok(seq) = seq_arc.lock() else { return };
        let desc = (&*seq, Some(Direction::Capture));
        let mut alsa_fds = vec![
            libc::pollfd {
                fd: 0,
                events: 0,
                revents: 0
            };
            desc.count()
        ];
        let _ = desc.fill(&mut alsa_fds);
        poll_fds.extend(alsa_fds);
    }

    let decoder = match MidiEvent::new(1024) {
        Ok(d) => d,
        Err(_) => return,
    };
    decoder.enable_running_status(false);

    let mut buffer = [0u8; 16];
    let mut sysex_buf = Vec::new();

    while running.load(Ordering::Relaxed) {
        let ret =
            unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as libc::nfds_t, 100) };

        if ret <= 0 {
            continue;
        }

        if poll_fds[0].revents & libc::POLLIN != 0 {
            break;
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }

        let Ok(seq) = seq_arc.lock() else { break };
        let mut input = seq.input();

        while let Ok(mut ev) = input.event_input() {
            let dest_port = ev.get_dest().port;
            let mut message = Vec::new();

            match ev.get_type() {
                EventType::PortSubscribed | EventType::PortUnsubscribed => {}
                EventType::Sysex => {
                    if let Some(ext) = ev.get_ext() {
                        sysex_buf.extend_from_slice(ext);
                        if sysex_buf.last() == Some(&0xF7) {
                            message = std::mem::take(&mut sysex_buf);
                        }
                    }
                }
                _ => {
                    if let Ok(nbytes) = decoder.decode(&mut buffer, &mut ev)
                        && nbytes > 0
                    {
                        message.extend_from_slice(&buffer[..nbytes]);
                    }
                }
            }

            if !message.is_empty() {
                let maybe_subs = ports_arc.lock().ok().and_then(|m| m.get(&dest_port).cloned());
                if let Some(subs) = maybe_subs {
                    dispatch_to_subscribers(&subs, &message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alsa_client_unification() {
        let logger = SharedLogger::new();
        let client = AlsaClient::new(logger.clone()).expect("Failed to create AlsaClient");
        let client_id = client.client_id;
        assert!(client_id > 0);

        let subs1: SubscribersList = Arc::new(Mutex::new(Vec::new()));
        let vport1 = client
            .create_virtual_port("ReMIDI [Main]", subs1)
            .expect("Failed to create virtual port 1");

        let subs2: SubscribersList = Arc::new(Mutex::new(Vec::new()));
        let vport2 = client
            .create_virtual_port("ReMIDI [Pedals]", subs2)
            .expect("Failed to create virtual port 2");

        assert_eq!(vport1.client.client_id, client_id);
        assert_eq!(vport2.client.client_id, client_id);
        assert_ne!(vport1.port_id, vport2.port_id);

        let seq = client.seq.lock().unwrap();
        let pinfo1 = seq
            .get_any_port_info(Addr {
                client: client_id,
                port: vport1.port_id,
            })
            .expect("Port 1 should exist on client");
        assert_eq!(pinfo1.get_client(), client_id);

        let pinfo2 = seq
            .get_any_port_info(Addr {
                client: client_id,
                port: vport2.port_id,
            })
            .expect("Port 2 should exist on client");
        assert_eq!(pinfo2.get_client(), client_id);
        drop(seq);

        // If Midi Through exists on this Linux system, test opening an output connection
        if let Ok(mut sender) = client.open_output_port("test_pipeline", "Midi Through") {
            assert_eq!(sender.sub.get_sender().client, client_id);
            // Test sending MIDI note on
            assert!(sender.send(&[0x90, 0x3C, 0x64]).is_ok());
        }
    }
}
