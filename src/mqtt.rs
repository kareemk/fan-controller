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

use crate::{execute, LoadedConfig, State};

/// HA's default MQTT Discovery prefix.
const DISCOVERY: &str = "homeassistant";
/// Base topic for this bridge's command/state/availability topics.
const BASE: &str = "fanctrl";
/// Fan speed levels this protocol supports (speed1..speed6).
const MAX_SPEED: u8 = 6;

struct FanState {
    on: bool,
    speed: u8,
    light: bool,
}

impl Default for FanState {
    fn default() -> Self {
        // A bare "on" has no protocol command, so default to a mid speed.
        FanState {
            on: false,
            speed: 3,
            light: false,
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

    fn has_fan(&self, name: &str) -> bool {
        self.config.fans.iter().any(|f| f.name == name)
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
        .publish(format!("{base}/state"), QoS::AtLeastOnce, true, if on { "ON" } else { "OFF" })
        .await;
    let _ = client
        .publish(format!("{base}/speed/state"), QoS::AtLeastOnce, true, speed.to_string())
        .await;
}

async fn publish_light_state(client: &AsyncClient, shared: &Arc<Shared>, name: &str) {
    let on = shared.fans.lock().unwrap().get(name).map(|s| s.light).unwrap_or(false);
    let _ = client
        .publish(
            format!("{BASE}/{name}/light/state"),
            QoS::AtLeastOnce,
            true,
            if on { "ON" } else { "OFF" },
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
    client.subscribe(format!("{BASE}/+/set"), QoS::AtLeastOnce).await?;
    client.subscribe(format!("{BASE}/+/speed/set"), QoS::AtLeastOnce).await?;
    client.subscribe(format!("{BASE}/+/light/set"), QoS::AtLeastOnce).await?;

    client
        .publish(format!("{BASE}/status"), QoS::AtLeastOnce, true, "online")
        .await?;

    // Seed each entity's state so HA shows something before the first command.
    for fan in &shared.config.fans {
        publish_fan_state(client, shared, &fan.name).await;
        if fan.has_light {
            publish_light_state(client, shared, &fan.name).await;
        }
    }
    Ok(())
}

async fn handle_publish(client: &AsyncClient, shared: &Arc<Shared>, topic: &str, payload: &[u8]) {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.first() != Some(&BASE) || parts.len() < 3 {
        return;
    }
    let name = parts[1];
    if !shared.has_fan(name) {
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
                let st = map.entry(name.to_string()).or_default();
                st.on = on;
                if on {
                    format!("speed{}", st.speed)
                } else {
                    "off".to_string()
                }
            };
            transmit(shared.clone(), name.to_string(), cmd).await;
            publish_fan_state(client, shared, name).await;
        }
        // Speed (HA sends an integer in the 1..MAX_SPEED range)
        (Some("speed"), Some("set")) => {
            let Ok(level) = payload.parse::<f32>() else { return };
            let level = level.round().clamp(1.0, MAX_SPEED as f32) as u8;
            {
                let mut map = shared.fans.lock().unwrap();
                let st = map.entry(name.to_string()).or_default();
                st.on = true;
                st.speed = level;
            }
            transmit(shared.clone(), name.to_string(), format!("speed{level}")).await;
            publish_fan_state(client, shared, name).await;
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
