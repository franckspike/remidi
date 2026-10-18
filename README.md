# ReMIDI

**ReMIDI** is a lightweight, cross-platform (**Windows** & **Linux**) system tray utility that remedies MIDI compatibility quirks by capturing, filtering, and transforming MIDI messages in real time across multiple pipelines.

Each pipeline creates a **virtual MIDI port** (visible to DAWs and other applications as a MIDI Output port), matches incoming messages against regular expressions on uppercase hexadecimal byte strings, applies transformations and optional debouncing, and forwards the resulting bytes to a real **MIDI Output port**.

---

## Features

- **Single User Configuration (`config.toml`):**
  - Stored automatically in your OS user configuration folder:
    - **Linux:** `~/.config/remidi/config.toml`
    - **Windows:** `%APPDATA%\remidi\config.toml`
  - Auto-created from the built-in template ([`config.example.toml`](config.example.toml)) on first run.
- **Multiple MIDI Pipelines in One Instance:**
  - Define as many `[[pipelines]]` as you need in `config.toml`.
  - Enable/disable individual pipelines, switch their output `target_port` from a live list of connected MIDI-out ports, or run their `test` sequence directly from the System Tray menu.
  - Changes to `enabled` or `target_port` made from the tray menu are automatically saved back to `config.toml` while preserving all comments and formatting.
- **Cross-Platform Virtual MIDI Loopback:**
  - **Linux:** Creates native ALSA/JACK virtual MIDI ports out of the box (no external drivers required).
  - **Windows:** Dynamically uses `teVirtualMIDI64.dll` (`teVirtualMIDISDK`, same driver used by `loopMIDI` and `rtpMIDI`) to create on-the-fly virtual MIDI ports.
- **Regex Hex Matching & Replacement:**
  - Incoming MIDI bytes are formatted as uppercase hex (`"C0 01"`, `"90 3C 7F"`).
  - Match patterns use standard regular expressions with capture groups (`$1`, `$2`, etc.).
- **Debouncing & Byte Adjustments:**
  - `min_delay_ms`: Filters out rapid duplicate/repeat triggers within the specified millisecond window.
  - `dec_second_byte`: Optionally decrements the second MIDI byte by 1 (e.g., converting 1-based Program Changes `01..80` to 0-based `00..7F`).
- **System Tray + Popup Log & Error Monitor:**
  - Runs quietly in the system tray with a 5-pin DIN MIDI icon (Green = active, Amber = paused, Red = error).
  - Automatically pops up the **Log & Error Monitor** window if an error occurs (such as a missing MIDI port or invalid rule).

---

## Configuration Format (`config.toml`)

See [`config.example.toml`](config.example.toml) for a fully commented example configuration:

```toml
[[pipelines]]
id = "Scene Change & Tap Tempo"
enabled = true
virtual_port = "ReMIDI"
target_port = "01. Ethernet MIDI"
test = ["C0 01", "sleep 1000", "C0 04"]

  [[pipelines.rules]]
  match = 'C0 (\w\w)'
  replace = "90 $1 7F"
  min_delay_ms = 500
  dec_second_byte = false
  comment = "Program Change -> Note On | Scene Change"

  [[pipelines.rules]]
  match = '90 01 \w\w'
  replace = "B0 50 00"
  min_delay_ms = 0
  comment = "Note On x01 -> Control Change 80 (dec) | Tap Tempo"
```

---

## Building & Installing (Windows & Linux)

### Prerequisites
- **Rust toolchain:** [rustup.rs](https://rustup.rs/)
- **Linux system packages:** ALSA & GTK3/AppIndicator development headers (`sudo apt install libasound2-dev libgtk-3-dev libayatana-appindicator3-dev` on Debian/Ubuntu)
- **Windows driver:** [teVirtualMIDI](https://www.tobias-erichsen.de/software/virtualmidi.html) (installed automatically with `loopMIDI` or `rtpMIDI`) so `teVirtualMIDI64.dll` is present in `C:\Windows\System32`.

### Commands (Same on Windows & Linux)

```sh
# 1. Build optimized release binary (target/release/remidi or remidi.exe)
cargo build --release

# 2. Run unit tests
cargo test

# 3. Install into your user PATH (~/.cargo/bin/remidi or %USERPROFILE%\.cargo\bin\remidi.exe)
cargo install --path .

# 4. Uninstall
cargo uninstall remidi
```

---

## System Tray Menu Actions

- **Show / Hide Log & Error Window:** Toggle the live log/error monitor window.
- **Per-Pipeline Submenus (`<id> (<virtual_port>)`):**
  - **Enabled:** Enable or disable the pipeline (saved to `config.toml`).
  - **Run Test:** Execute the pipeline's `test = [...]` sequence.
  - **MIDI-Out Port List:** Select the destination `target_port` from all currently detected MIDI-out ports (saved to `config.toml`).
- **Edit Config (`edit`):** Open `config.toml` in `$VISUAL` / `$EDITOR` (or `notepad` on Windows / `vi` in a terminal on Linux), wait until the editor closes, and automatically reload.
- **Reload Config (`update`):** Re-parse and reload `config.toml`.
- **Verbose Logging (`verbose`):** Log unmatched and delay-filtered MIDI messages in addition to matched transformations.
- **Pause All Pipelines:** Temporarily mute MIDI processing across all pipelines without closing virtual ports.
- **Quit ReMIDI:** Close virtual/hardware MIDI ports and exit.
