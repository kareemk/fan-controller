# Fan controller for home automation

Transmit and receive OOK (on-off keying) signals to control ceiling fans at 433 MHz using a software-defined radio (BladeRF, HackRF, or any SoapySDR-compatible device).

Compatible with a FT0317A controllers.

## Binaries

## fan-tx

Transmit fan remote commands. Supports two vendor protocols (gap-width encoding and pulse-width encoding).

### HTTP API

When running with `--http-server`:

- `GET /targets` — returns JSON with available fans, rooms, and commands
- `POST /send?target=<target>&cmd=<command>` — send a command to a target

```bash
# List available fans, rooms, and commands
curl http://localhost:8080/targets

# Turn all fans off
curl -X POST 'http://localhost:8080/send?target=*&cmd=off'

# Set a specific fan to speed 3
curl -X POST 'http://localhost:8080/send?target=palapa1&cmd=speed3'

# Toggle light for a room
curl -X POST 'http://localhost:8080/send?target=palapa&cmd=toggle_light'
```

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

# HTTP server mode
cargo run --release --bin fan-tx -- --driver hackrf --http-server 0.0.0.0:8080
```

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `-g, --gain` | 70.0 | TX gain in dB |
| `--driver` | _(required)_ | SoapySDR driver name (e.g. `hackrf`, `bladerf`) |
| `-c, --config` | config.yaml | Path to config file |
| `--repeat` | 2 | Times to repeat each command |
| `--http-server` | — | Start HTTP server on IP:PORT |

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
