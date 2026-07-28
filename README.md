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

# MQTT bridge for Home Assistant (see the Home Assistant section)
cargo run --release --bin fan-tx -- --driver hackrf --mqtt
```

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `-g, --gain` | 70.0 | TX gain in dB |
| `--driver` | _(required)_ | SoapySDR driver name (e.g. `hackrf`, `bladerf`) |
| `-c, --config` | config.yaml | Path to config file |
| `--repeat` | 2 | Times to repeat each command |
| `--mcp` | — | Start an MCP server over stdio |
| `--mqtt` | — | Start an MQTT bridge for Home Assistant |

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

## Home Assistant

`fan-tx --mqtt` bridges the fans into Home Assistant over MQTT using HA's
[MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery):
each fan appears automatically as a **fan** entity (on/off + 6-step speed), and
fans marked `light: true` also get a **light** entity. It holds the SDR open, so
it runs on the machine the HackRF is attached to. Everyone in the house controls
the fans from the free Home Assistant app — no HomeKit or hub required.

Each fan also publishes numeric preset modes `1`–`6`. Add the **Fan preset
modes** feature to a Home Assistant tile card to show the protocol's discrete
speed levels without relying on the percentage slider. The included
[`homeassistant/fan-dashboard.yaml`](homeassistant/fan-dashboard.yaml) provides
a mobile-friendly dashboard with explicit **Off, 1, 2, 3, 4, 5, 6** buttons for
the Palapa group, Galleria group, and Guest fan. Group buttons publish one MQTT
room command, so the bridge sends every room member in a single ordered RF
transmission using the same timing as the CLI room command.

### Quick start (always-on Mac mini)

The [`homeassistant/`](homeassistant/) directory has a Docker stack for Home
Assistant + a Mosquitto broker. The bridge runs natively (Docker on macOS can't
pass through the USB HackRF) and talks to the broker on `localhost`.

```bash
# Keep the Mac available as a server while allowing display sleep
sudo pmset -a sleep 0 autorestart 1 womp 1

# Start Colima at login using its supported foreground service
brew services start colima

# 1. Start Home Assistant + the MQTT broker
cd homeassistant && docker compose up -d

# 2. Run the bridge natively (connects to the broker, registers the fans)
cargo run --release --bin fan-tx -- --driver hackrf --mqtt

# 3. Open http://<mac-mini>:8123, create your account, then add the MQTT
#    integration (Settings -> Devices & Services -> Add Integration -> MQTT)
#    with broker host `mosquitto`, port `1883`.
```

The fans then appear under Settings → Devices & Services → MQTT. Add family
members under Settings → People; for remote access put WireGuard or Tailscale on
the Mac mini. To run the bridge as a boot service, use the launchd template in
[`launchd/com.fan-controller.mqtt.plist`](launchd/com.fan-controller.mqtt.plist).
The launchd bridge template sends two complete RF bursts per command, matching
the CLI default.

Broker connection flags: `--mqtt-host` (default `localhost`), `--mqtt-port`
(default `1883`), and `--mqtt-user` / `--mqtt-pass` if the broker isn't anonymous.

If the HackRF is unplugged while the MQTT bridge is running, its active USB
stream becomes invalid. The bridge exits on the next failed transmission so
launchd can restart it and reopen the HackRF after it is reconnected.

### Entity mapping

| HA control | Command sent |
|---|---|
| Fan off, or speed 0 | `off` |
| Fan on | `speed3` (the protocol has no bare "on") |
| Fan speed slider or numeric preset 1–6 | `speed1`–`speed6` |
| Fan direction | `forward`/`reverse` (Vendor A) or `toggle_direction` (Vendor B) |
| Light on/off | `toggle_light` |

State is optimistic (the fans give no feedback), and the light is a toggle, so
the bridge tracks assumed light state and only transmits when HA's target
differs — using the physical remote can desync it until the next toggle.

## Configuration

Fans and rooms are defined in `config.yaml`:

```yaml
fans:
  - { name: palapa1, vendor: vendor_a, device_id: 0x87552 }
  - { name: galleria1, vendor: vendor_b, device_id: 0xED13F, light: true }  # has a physical light

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
