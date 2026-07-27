// -- MQTT bridge for Home Assistant --
//
// Publishes Home Assistant MQTT Discovery configs so each configured fan shows
// up automatically as a `fan` entity (on/off + 6-step speed), plus a `light`
// entity for fans with `light: true`. Subscribes to the command topics HA
// publishes to and routes each command through the shared SDR transmit path
// (mutex-guarded, run on the blocking pool so the MQTT loop stays responsive).
//
// Fans are stateless (one-way RF), so state is optimistic: we echo back what we
// last commanded. The light command is a toggle, so we track assumed light
// state and only transmit when HA's target differs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use num::complex::Complex32 as c32;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::json;

use crate::{execute, resolve_target, LoadedConfig, State};

/// HA's default MQTT Discovery prefix.
const DISCOVERY: &str = "homeassistant";
/// Base topic for this bridge's command/state/availability topics.
const BASE: &str = "fanctrl";
/// Fan speed levels this protocol supports (speed1..speed6).
const MAX_SPEED: u8 = 6;
/// Numeric preset labels let Home Assistant render the discrete protocol
/// speeds as buttons instead of only exposing its percentage slider.
const SPEED_PRESETS: [&str; MAX_SPEED as usize] = ["1", "2", "3", "4", "5", "6"];

struct FanState {
    on: bool,
    speed: u8,
    light: bool,
    /// Assumed direction; true = forward. Tracked for state and for the
    /// Vendor B toggle (which has no absolute set).
    forward: bool,
}

impl Default for FanState {
    fn default() -> Self {
        // A bare "on" has no protocol command, so default to a mid speed.
        FanState {
            on: false,
            speed: 3,
            light: false,
            forward: true,
        }
    }
}

struct Shared {
    config: LoadedConfig,
    stream: Mutex<soapysdr::TxStream<c32>>,
    state: Mutex<State>,
    repeat: usize,
    fans: Mutex<HashMap<String, FanState>>,
}

impl Shared {
    fn send(&self, target: &str, cmd: &str) -> Result<()> {
        let input = format!("{target} {cmd}");
        let mut stream = self.stream.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        execute(&self.config, &mut stream, &mut state, &input, self.repeat)
    }

    /// Resolve an MQTT target to the fan state entries it affects. A target
    /// can be one physical fan or a configured CLI room such as `palapa`.
    fn target_fan_names(&self, target: &str) -> Vec<String> {
        resolve_target(&self.config, target)
            .unwrap_or_default()
            .into_iter()
            .map(|fan| fan.name.clone())
            .collect()
    }
}

/// Run `execute` off the async runtime; it blocks (SDR I/O plus a 1s sleep
/// between repeats).
async fn transmit(shared: Arc<Shared>, target: String, cmd: String) {
    match tokio::task::spawn_blocking(move || shared.send(&target, &cmd)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[mqtt] transmit failed: {e:#}"),
        Err(e) => eprintln!("[mqtt] transmit task panicked: {e}"),
    }
}

async fn publish_fan_state(client: &AsyncClient, shared: &Arc<Shared>, name: &str) {
    let (on, speed) = {
        let map = shared.fans.lock().unwrap();
        map.get(name).map(|s| (s.on, s.speed)).unwrap_or((false, 3))
    };
    let base = format!("{BASE}/{name}");
    let _ = client
        .publish(
            format!("{base}/state"),
            QoS::AtLeastOnce,
            true,
            if on { "ON" } else { "OFF" },
        )
        .await;
    let _ = client
        .publish(
            format!("{base}/speed/state"),
            QoS::AtLeastOnce,
            true,
            speed.to_string(),
        )
        .await;
    let _ = client
        .publish(
            format!("{base}/preset/state"),
            QoS::AtLeastOnce,
            true,
            speed.to_string(),
        )
        .await;
}

async fn publish_light_state(client: &AsyncClient, shared: &Arc<Shared>, name: &str) {
    let on = shared
        .fans
        .lock()
        .unwrap()
        .get(name)
        .map(|s| s.light)
        .unwrap_or(false);
    let _ = client
        .publish(
            format!("{BASE}/{name}/light/state"),
            QoS::AtLeastOnce,
            true,
            if on { "ON" } else { "OFF" },
        )
        .await;
}

async fn publish_direction_state(client: &AsyncClient, shared: &Arc<Shared>, name: &str) {
    let forward = shared
        .fans
        .lock()
        .unwrap()
        .get(name)
        .map(|s| s.forward)
        .unwrap_or(true);
    let _ = client
        .publish(
            format!("{BASE}/{name}/direction/state"),
            QoS::AtLeastOnce,
            true,
            if forward { "forward" } else { "reverse" },
        )
        .await;
}

/// (re)publish discovery configs, subscribe to command topics, and announce
/// availability. Runs on every ConnAck so it survives reconnects.
async fn setup(client: &AsyncClient, shared: &Arc<Shared>) -> Result<()> {
    for fan in &shared.config.fans {
        let name = &fan.name;
        let base = format!("{BASE}/{name}");
        let device = json!({
            "identifiers": [format!("fanctrl_{name}")],
            "name": name,
            "manufacturer": "fan-controller",
            "model": "OOK-433",
        });

        let fan_cfg = json!({
            "name": serde_json::Value::Null, // use the device name for the primary entity
            "unique_id": format!("fanctrl_{name}"),
            "command_topic": format!("{base}/set"),
            "state_topic": format!("{base}/state"),
            "percentage_command_topic": format!("{base}/speed/set"),
            "percentage_state_topic": format!("{base}/speed/state"),
            "speed_range_min": 1,
            "speed_range_max": MAX_SPEED,
            "preset_mode_command_topic": format!("{base}/preset/set"),
            "preset_mode_state_topic": format!("{base}/preset/state"),
            "preset_modes": SPEED_PRESETS,
            "direction_command_topic": format!("{base}/direction/set"),
            "direction_state_topic": format!("{base}/direction/state"),
            "availability_topic": format!("{BASE}/status"),
            "device": device,
        });
        client
            .publish(
                format!("{DISCOVERY}/fan/fanctrl/{name}/config"),
                QoS::AtLeastOnce,
                true,
                fan_cfg.to_string(),
            )
            .await
            .context("publish fan discovery")?;

        if fan.has_light {
            let light_cfg = json!({
                "name": "Light",
                "unique_id": format!("fanctrl_{name}_light"),
                "command_topic": format!("{base}/light/set"),
                "state_topic": format!("{base}/light/state"),
                "availability_topic": format!("{BASE}/status"),
                "device": device,
            });
            client
                .publish(
                    format!("{DISCOVERY}/light/fanctrl/{name}_light/config"),
                    QoS::AtLeastOnce,
                    true,
                    light_cfg.to_string(),
                )
                .await
                .context("publish light discovery")?;
        }
    }

    // HA publishes commands to these; `+` is one fan name.
    client
        .subscribe(format!("{BASE}/+/set"), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(format!("{BASE}/+/speed/set"), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(format!("{BASE}/+/preset/set"), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(format!("{BASE}/+/light/set"), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(format!("{BASE}/+/direction/set"), QoS::AtLeastOnce)
        .await?;

    client
        .publish(format!("{BASE}/status"), QoS::AtLeastOnce, true, "online")
        .await?;

    // Seed each entity's state so HA shows something before the first command.
    for fan in &shared.config.fans {
        publish_fan_state(client, shared, &fan.name).await;
        publish_direction_state(client, shared, &fan.name).await;
        if fan.has_light {
            publish_light_state(client, shared, &fan.name).await;
        }
    }
    Ok(())
}

fn parse_speed_level(payload: &str) -> Option<u8> {
    let level = payload.parse::<f32>().ok()?;
    level
        .is_finite()
        .then(|| level.round().clamp(1.0, MAX_SPEED as f32) as u8)
}

async fn handle_publish(client: &AsyncClient, shared: &Arc<Shared>, topic: &str, payload: &[u8]) {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.first() != Some(&BASE) || parts.len() < 3 {
        return;
    }
    let name = parts[1];
    let target_fans = shared.target_fan_names(name);
    if target_fans.is_empty() {
        return;
    }
    let payload = String::from_utf8_lossy(payload);
    let payload = payload.trim();

    match (parts.get(2).copied(), parts.get(3).copied()) {
        // Power on/off
        (Some("set"), None) => {
            let on = payload.eq_ignore_ascii_case("ON");
            let cmd = {
                let mut map = shared.fans.lock().unwrap();
                let target_state = map.entry(name.to_string()).or_default();
                target_state.on = on;
                if on {
                    let speed = target_state.speed;
                    for fan_name in &target_fans {
                        let st = map.entry(fan_name.clone()).or_default();
                        st.on = true;
                        st.speed = speed;
                    }
                    format!("speed{speed}")
                } else {
                    for fan_name in &target_fans {
                        map.entry(fan_name.clone()).or_default().on = false;
                    }
                    "off".to_string()
                }
            };
            transmit(shared.clone(), name.to_string(), cmd).await;
            for fan_name in &target_fans {
                publish_fan_state(client, shared, fan_name).await;
            }
        }
        // Speed (HA sends an integer in the 1..MAX_SPEED range)
        (Some("speed"), Some("set")) => {
            let Some(level) = parse_speed_level(payload) else {
                return;
            };
            {
                let mut map = shared.fans.lock().unwrap();
                let target_state = map.entry(name.to_string()).or_default();
                target_state.on = true;
                target_state.speed = level;
                for fan_name in &target_fans {
                    let st = map.entry(fan_name.clone()).or_default();
                    st.on = true;
                    st.speed = level;
                }
            }
            transmit(shared.clone(), name.to_string(), format!("speed{level}")).await;
            for fan_name in &target_fans {
                publish_fan_state(client, shared, fan_name).await;
            }
        }
        // Numeric preset buttons map directly onto the protocol's speed1..speed6.
        (Some("preset"), Some("set")) => {
            let Some(level) = parse_speed_level(payload) else {
                return;
            };
            {
                let mut map = shared.fans.lock().unwrap();
                let target_state = map.entry(name.to_string()).or_default();
                target_state.on = true;
                target_state.speed = level;
                for fan_name in &target_fans {
                    let st = map.entry(fan_name.clone()).or_default();
                    st.on = true;
                    st.speed = level;
                }
            }
            transmit(shared.clone(), name.to_string(), format!("speed{level}")).await;
            for fan_name in &target_fans {
                publish_fan_state(client, shared, fan_name).await;
            }
        }
        // Light on/off (toggle-diff against assumed state)
        (Some("light"), Some("set")) => {
            let on = payload.eq_ignore_ascii_case("ON");
            let toggle = {
                let mut map = shared.fans.lock().unwrap();
                let st = map.entry(name.to_string()).or_default();
                if st.light != on {
                    st.light = on;
                    true
                } else {
                    false
                }
            };
            if toggle {
                transmit(shared.clone(), name.to_string(), "toggle_light".to_string()).await;
            }
            publish_light_state(client, shared, name).await;
        }
        // Direction forward/reverse
        (Some("direction"), Some("set")) => {
            let want_forward = payload.eq_ignore_ascii_case("forward");
            // Vendor A fans have absolute forward/reverse buttons; Vendor B only
            // has toggle_direction, so track assumed direction and toggle on change.
            let absolute = shared
                .config
                .fans
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.buttons.iter().any(|(n, _)| *n == "forward"))
                .unwrap_or(false);
            let cmd = {
                let mut map = shared.fans.lock().unwrap();
                let st = map.entry(name.to_string()).or_default();
                if absolute {
                    st.forward = want_forward;
                    Some(if want_forward { "forward" } else { "reverse" }.to_string())
                } else if st.forward != want_forward {
                    st.forward = want_forward;
                    Some("toggle_direction".to_string())
                } else {
                    None
                }
            };
            if let Some(cmd) = cmd {
                transmit(shared.clone(), name.to_string(), cmd).await;
            }
            publish_direction_state(client, shared, name).await;
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: LoadedConfig,
    stream: soapysdr::TxStream<c32>,
    state: State,
    repeat: usize,
    host: &str,
    port: u16,
    user: Option<String>,
    pass: Option<String>,
) -> Result<()> {
    let n_fans = config.fans.len();
    let shared = Arc::new(Shared {
        config,
        stream: Mutex::new(stream),
        state: Mutex::new(state),
        repeat,
        fans: Mutex::new(HashMap::new()),
    });

    let mut opts = MqttOptions::new("fan-controller", host, port);
    opts.set_keep_alive(Duration::from_secs(30));
    // Broker marks us offline (via HA availability) if we drop.
    opts.set_last_will(LastWill::new(
        format!("{BASE}/status"),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    if let Some(u) = user {
        opts.set_credentials(u, pass.unwrap_or_default());
    }

    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    eprintln!("[mqtt] connecting to {host}:{port} ...");

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => match setup(&client, &shared).await {
                Ok(()) => eprintln!(
                    "[mqtt] connected to {host}:{port}; advertised {n_fans} fans to Home Assistant"
                ),
                Err(e) => eprintln!("[mqtt] setup failed: {e:#}"),
            },
            Ok(Event::Incoming(Packet::Publish(p))) => {
                handle_publish(&client, &shared, &p.topic, &p.payload).await;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[mqtt] connection error: {e}; retrying...");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_speed_level;

    #[test]
    fn parses_and_clamps_speed_levels() {
        assert_eq!(parse_speed_level("1"), Some(1));
        assert_eq!(parse_speed_level("3.4"), Some(3));
        assert_eq!(parse_speed_level("6"), Some(6));
        assert_eq!(parse_speed_level("0"), Some(1));
        assert_eq!(parse_speed_level("7"), Some(6));
        assert_eq!(parse_speed_level("nope"), None);
        assert_eq!(parse_speed_level("NaN"), None);
    }
}
