# Fan controller for home automation

Transmit and receive OOK (on-off keying) signals to control ceiling fans at 433 MHz using a software-defined radio (BladeRF, HackRF, or any SoapySDR-compatible device).

Compatible with a FT0317A controllers.

## Binaries

## fan-tx

Transmit fan remote commands. Supports two vendor protocols (gap-width encoding and pulse-width encoding).

### Shell

`--driver` is required — set it to your SDR's SoapySDR driver (e.g. `hackrf`, `bladerf`).

```bash
# One-shot command: turn all fans off
cargo run --release --bin fan-tx -- --driver hackrf '*' off

# Target a specific fan
cargo run --release --bin fan-tx -- --driver hackrf palapa1 speed3

# Target a room (glob pattern from config)
cargo run --release --bin fan-tx -- --driver hackrf 'palapa*' toggle_light

# Interactive mode (stdin)
cargo run --release --bin fan-tx -- --driver hackrf

# MCP server over stdio (for AI agents)
cargo run --release --bin fan-tx -- --driver hackrf --mcp

# HomeKit bridge (see the HomeKit section below)
cargo run --release --bin fan-tx -- --driver hackrf --homekit
```

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `-g, --gain` | 70.0 | TX gain in dB |
| `--driver` | _(required)_ | SoapySDR driver name (e.g. `hackrf`, `bladerf`) |
| `-c, --config` | config.yaml | Path to config file |
| `--repeat` | 2 | Times to repeat each command |
| `--mcp` | — | Start an MCP server over stdio |
| `--homekit` | — | Start a HomeKit bridge (HAP) |
| `--homekit-pin` | 11122333 | HomeKit pairing code (8 digits) |

**Available commands (Vendor A):** `off`, `speed1`–`speed6`, `fan_off`, `toggle_light`, `forward`, `reverse`, `breeze`, `1h`, `4h`, `8h`

**Available commands (Vendor B):** `off`, `speed1`–`speed6`, `fan_off`, `toggle_light`, `toggle_direction`, `breeze`, `home_shield`, `1h`, `4h`, `8h`

### fan-rx

Listen for and decode fan remote OOK codes. Useful for capturing device IDs from existing remotes.

```bash
# Listen (--driver is required)
cargo run --release --bin fan-rx -- --driver hackrf

# Adjust gain
cargo run --release --bin fan-rx -- --driver hackrf --gain 50

# Calibration mode (print amplitude stats)
cargo run --release --bin fan-rx -- --driver hackrf --calibrate
```

## HomeKit

`fan-tx --homekit` runs a HomeKit bridge (via a [patched fork](https://github.com/kareemk/hap-rs) of [hap-rs](https://github.com/ewilken/hap-rs)) that exposes each configured fan as a HomeKit **Fan** accessory (on/off + 6-step speed) and a **Lightbulb**. It holds the SDR open and routes every characteristic change through the transmitter, so it must run on the machine the SDR is attached to.

```bash
cargo run --release --bin fan-tx -- --driver hackrf --homekit
```

Then in the iOS **Home** app: **Add Accessory → More options… → Fan Controller**, accept the "Uncertified Accessory" prompt, and enter the pairing code (default `111-22-333`; change with `--homekit-pin`).

**Accessory mapping**

| HomeKit control | Command sent |
|---|---|
| Fan off, or speed slider to 0 | `off` |
| Fan on (power button) | `speed3` (the protocol has no bare "on") |
| Fan speed slider 1–100% | `speed1`–`speed6` |
| Lightbulb on/off | `toggle_light` (see caveats) |

**Caveats**

- The fans are stateless (one-way RF, no feedback), so HomeKit shows *optimistic* state — what it last commanded, not what the fan is actually doing.
- The light command is a **toggle**, not absolute on/off. The bridge tracks the assumed light state and only transmits when HomeKit's target differs. Using the physical remote can desync that assumption — toggle the light once in the Home app to resync.

**Run it as a service (always-on Mac mini)**

A `launchd` template is in [`launchd/com.fan-controller.homekit.plist`](launchd/com.fan-controller.homekit.plist). Edit the `__REPO__` paths, then:

```bash
cp launchd/com.fan-controller.homekit.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.fan-controller.homekit.plist
tail -f homekit.log
```

Pairing state is stored under `~/.fan-controller-homekit/`; delete that directory to factory-reset (unpair) the bridge.

## Configuration

Fans and rooms are defined in `config.yaml`:

```yaml
fans:
  - { name: palapa1, vendor: vendor_a, device_id: 0x87552 }
  - { name: galleria1, vendor: vendor_b, device_id: 0xED13F }

rooms:
  main: "*"
  palapa: "palapa*"
```

## Building

### Native

Requires [SoapySDR](https://github.com/pothosware/SoapySDR) plus the driver module for your SDR (e.g. SoapyHackRF, SoapyBladeRF). The `soapysdr` crate finds SoapySDR via `pkg-config` at build time, so the library must be installed before `cargo build`.

macOS (Homebrew):

```bash
brew install soapysdr soapyhackrf   # HackRF; BladeRF needs SoapyBladeRF built from source
cargo build --release
```

Verify SoapySDR sees your device driver and hardware:

```bash
SoapySDRUtil --info                 # should list your module under "Available factories"
SoapySDRUtil --find="driver=hackrf" # should find the connected device
```

### Docker

```bash
docker build -t fan-controller .
```

Note: Docker on macOS cannot pass through USB devices directly. Run natively on macOS, or use Docker on Linux:

```bash
docker run --device /dev/bus/usb -v ./config.yaml:/config.yaml fan-controller
```

## Troubleshooting

**Build fails: `The system library SoapySDR ... was not found`** — the native SoapySDR
library isn't installed (or not on the `pkg-config` path). Install it: `brew install soapysdr`.

**Runtime: `SoapySDR::Device::make() no match`** — SoapySDR is installed but has no driver
module for your SDR, or no device is connected. Install the module (`brew install soapyhackrf`)
and confirm both the module and hardware are visible:

```bash
SoapySDRUtil --info                 # "Available factories" should list your driver (e.g. hackrf)
SoapySDRUtil --find="driver=hackrf" # should show the connected device
```

**Transmits without error but the fans don't respond** — the hardware is likely fine; it's a
signal issue. Check, in order:

1. A 433 MHz antenna is on the TX/RX port (range is very short without one).
2. Gain is within the device's range. HackRF TX tops out at 61 dB (the tool clamps and logs
   when a higher value is requested); BladeRF goes much higher.
3. `SAMPLE_RATE` isn't underrunning the USB bus. HackRF sharing a bus with other devices can't
   sustain high rates — TX underruns corrupt the OOK pulse timing so the fan can't decode. This
   tool transmits at 2 Msps for that reason. Raise it only if your bus can keep up.
4. `--repeat 5` to rule out marginal reception.

**Verify the radio independently of this tool.** For HackRF, `hackrf_info` confirms USB/firmware,
and `hackrf_transfer` confirms it actually radiates RF (bypassing SoapySDR and this code):

```bash
hackrf_info                                                         # firmware + board ID over USB
hackrf_transfer -t /dev/zero -f 433900000 -s 2000000 -x 40 -a 1     # exercise the TX chain
```
