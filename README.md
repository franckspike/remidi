# ReMIDI

**ReMIDI** is a lightweight, cross-platform (**Windows** & **Linux**) system tray utility that remedies MIDI compatibility quirks by capturing, filtering, and transforming MIDI messages in real time.

It creates a **virtual MIDI port** (visible to DAWs and other applications as a MIDI Output port), matches incoming messages against regular expressions on uppercase hexadecimal byte strings, applies transformations and optional debouncing, and forwards the resulting bytes to a real **MIDI Output port**.

---

## Features

- **Cross-Platform Virtual MIDI Loopback:**
  - **Linux:** Creates native ALSA/JACK virtual MIDI ports out of the box (no external drivers required).
  - **Windows:** Dynamically uses `teVirtualMIDI64.dll` (`teVirtualMIDISDK`, same driver used by `loopMIDI` and `rtpMIDI`) to create on-the-fly virtual MIDI ports, or falls back to opening an existing loopback port.
- **Regex Hex Matching & Replacement:**
  - Incoming MIDI bytes are formatted as uppercase hex (`"C0 01"`, `"90 3C 7F"`).
  - Match patterns use standard regular expressions with capture groups (`$1`, `$2`, etc.).
- **Debouncing & Byte Adjustments:**
  - `min_delay_ms`: Filters out rapid duplicate/repeat triggers within the specified millisecond window.
  - `dec_second_byte`: Optionally decrements the second MIDI byte by 1 (e.g., converting 1-based Program Changes `01..80` to 0-based `00..7F`).
- **Built-in Test Sequence Runner:**
  - Define test `send` and `sleep_ms` steps right inside your `.remidi` config file and trigger them from the system tray menu or log window.
- **System Tray + Popup Log & Error Monitor:**
  - Runs quietly in the system tray with a 5-pin DIN MIDI icon (Green = active, Amber = paused, Red = error).
  - Automatically pops up the **Log & Error Monitor** window if an error occurs (such as a missing MIDI port or invalid rule).
  - Supports **TOML** `.remidi` files (recommended) as well as legacy JSON `.remidi` files.

---

## Configuration Format (`.remidi`)

See [`Sample.remidi`](Sample.remidi) for a fully commented example configuration:

```toml
[[filters]]
id = "program change to (note on) scene change"
virtual_port = "ReMIDI"
target_port = "01. Ethernet MIDI"

  [[filters.rules]]
  match = 'C0 (\w\w)'
  replace = "90 $1 7F"
  min_delay_ms = 500
  dec_second_byte = false
  comment = "Program Change -> Note On | Scene Change"

  [[filters.rules]]
  match = '90 01 \w\w'
  replace = "B0 50 00"
  min_delay_ms = 0
  comment = "Note On x01 -> Control Change 80 (dec) | Tap Tempo"

[[tests]]
send = "C0 01"

[[tests]]
sleep_ms = 1000

[[tests]]
send = "C0 04"
```

---

## Building & Installing (Windows & Linux)

In Rust, **`cargo`** handles configuration, compilation, testing, and installation identically on both Windows and Linux:

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

## Usage

```sh
# Launch with a specific .remidi configuration file:
remidi path/to/profile.remidi

# Or launch with default discovery (loads ./Sample.remidi if present):
remidi
```

### System Tray Menu Actions
- **Show / Hide Log & Error Window:** Toggle the live log/error monitor window.
- **Load .remidi Config...:** Open a file dialog to switch `.remidi` configurations.
- **Quick Configs:** Submenu listing `.remidi` files discovered in the current or executable directory.
- **Edit Current Config (`edit`):** Open the active `.remidi` file in `$VISUAL` / `$EDITOR` (or `notepad` on Windows / `vi` in a terminal on Linux), wait until the editor closes, and automatically reload the configuration.
- **Reload Current Config (`update`):** Re-parse and reload the active `.remidi` file.
- **Run Test Sequence (`test`):** Execute the `[[tests]]` steps defined in the active config.
- **List Available MIDI Ports (`list`):** Print all detected system MIDI ports to the log window.
- **Verbose Logging (`verbose`):** Log unmatched and delay-filtered MIDI messages in addition to matched transformations.
- **Pause MIDI Processing:** Temporarily bypass/mute MIDI processing without closing the virtual port.
- **Quit ReMIDI:** Close virtual/hardware MIDI ports and exit.
