// -- HomeKit (HAP) bridge --
//
// Exposes each configured fan as a HomeKit Fan accessory (on/off + 6-step
// speed) plus a Lightbulb accessory, routing every characteristic change
// through the shared SDR transmit path. The transmit path is mutex-guarded so
// HomeKit's concurrent callbacks serialize onto the single TX stream, mirroring
// the MCP handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use num::complex::Complex32 as c32;

use hap::{
    accessory::{
        bridge::BridgeAccessory, fan::FanAccessory, lightbulb::LightbulbAccessory,
        AccessoryCategory, AccessoryInformation,
    },
    characteristic::AsyncCharacteristicCallbacks,
    futures::future::FutureExt,
    server::{IpServer, Server},
    storage::{FileStorage, Storage},
    Config, MacAddress, Pin,
};

use crate::{execute, LoadedConfig, State};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

struct Shared {
    config: LoadedConfig,
    stream: Mutex<soapysdr::TxStream<c32>>,
    state: Mutex<State>,
    repeat: usize,
    /// Assumed on/off state per fan light. The remote only exposes a toggle
    /// (no absolute set), so we track what we last sent and only transmit when
    /// HomeKit's desired state differs. Drifts if the physical remote is used.
    light_on: Mutex<HashMap<String, bool>>,
}

impl Shared {
    fn send(&self, target: &str, cmd: &str) -> Result<()> {
        let input = format!("{target} {cmd}");
        let mut stream = self.stream.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        execute(&self.config, &mut stream, &mut state, &input, self.repeat)
    }
}

/// Map a HomeKit RotationSpeed percentage (0-100) to a fan command.
fn speed_cmd(percent: f32) -> String {
    if percent <= 0.0 {
        "off".to_string()
    } else {
        let level = ((percent / 100.0) * 6.0).round().clamp(1.0, 6.0) as u8;
        format!("speed{level}")
    }
}

/// Transmit off the async runtime: `execute` blocks (SDR I/O plus a 1s sleep
/// between repeats), so run it on the blocking pool to keep the HAP server
/// responsive.
async fn transmit(shared: Arc<Shared>, target: String, cmd: String) {
    match tokio::task::spawn_blocking(move || shared.send(&target, &cmd)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[homekit] transmit failed: {e:#}"),
        Err(e) => eprintln!("[homekit] transmit task panicked: {e}"),
    }
}

pub async fn run(
    config: LoadedConfig,
    stream: soapysdr::TxStream<c32>,
    state: State,
    repeat: usize,
    pin: [u8; 8],
    name: &str,
) -> Result<()> {
    let shared = Arc::new(Shared {
        config,
        stream: Mutex::new(stream),
        state: Mutex::new(state),
        repeat,
        light_on: Mutex::new(HashMap::new()),
    });

    // Persist pairing/config under the home dir so it survives restarts and is
    // independent of the process's working directory (important under launchd).
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = std::path::PathBuf::from(home).join(".fan-controller-homekit");
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let mut storage = FileStorage::new(&dir)
        .await
        .context("Failed to open HomeKit storage")?;

    let hap_config = match storage.load_config().await {
        Ok(mut c) => {
            c.redetermine_local_ip();
            c.pin = Pin::new(pin).map_err(|e| anyhow::anyhow!("Invalid PIN: {e}"))?;
            storage.save_config(&c).await.ok();
            c
        }
        Err(_) => {
            let c = Config {
                pin: Pin::new(pin).map_err(|e| anyhow::anyhow!("Invalid PIN: {e}"))?,
                name: name.to_string(),
                device_id: MacAddress::from([0xFA, 0x11, 0xC0, 0x27, 0x00, 0x01]),
                category: AccessoryCategory::Bridge,
                ..Default::default()
            };
            storage.save_config(&c).await.ok();
            c
        }
    };

    let server = IpServer::new(hap_config, storage)
        .await
        .context("Failed to start HomeKit server")?;

    // Bridge is accessory id 1; children get unique ids after it.
    let bridge = BridgeAccessory::new(
        1,
        AccessoryInformation {
            name: name.to_string(),
            manufacturer: "fan-controller".into(),
            model: "OOK-433".into(),
            serial_number: "fan-controller-bridge".into(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("bridge: {e}"))?;
    server
        .add_accessory(bridge)
        .await
        .map_err(|e| anyhow::anyhow!("add bridge: {e}"))?;

    for (i, fan) in shared.config.fans.iter().enumerate() {
        let fan_name = fan.name.clone();
        let serial = format!("{:05X}", fan.device_id);

        // Deterministic ids reserve two slots per fan (fan, then its light), so
        // toggling a fan's `light` never renumbers the other accessories and
        // HomeKit keeps their identity, rooms, and names. Bridge is id 1.
        let fan_aid = 2 + (i as u64) * 2;
        let light_aid = fan_aid + 1;

        // -- Fan accessory: power + 6-step speed --
        let mut fan_acc = FanAccessory::new(
            fan_aid,
            AccessoryInformation {
                name: fan_name.clone(),
                manufacturer: "fan-controller".into(),
                model: "OOK-433".into(),
                serial_number: format!("{serial}-fan"),
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("fan {fan_name}: {e}"))?;

        {
            let sh = shared.clone();
            let target = fan_name.clone();
            fan_acc
                .fan
                .power_state
                .on_update_async(Some(move |_old: bool, new: bool| {
                    let sh = sh.clone();
                    let target = target.clone();
                    async move {
                        // The protocol has no bare "on"; default to a mid speed.
                        let cmd = if new { "speed3" } else { "off" };
                        transmit(sh, target, cmd.to_string()).await;
                        Ok::<(), BoxErr>(())
                    }
                    .boxed()
                }));
        }
        {
            let sh = shared.clone();
            let target = fan_name.clone();
            fan_acc
                .fan
                .rotation_speed
                .as_mut()
                .expect("rotation_speed present by default")
                .on_update_async(Some(move |_old: f32, new: f32| {
                    let sh = sh.clone();
                    let target = target.clone();
                    async move {
                        transmit(sh, target, speed_cmd(new)).await;
                        Ok::<(), BoxErr>(())
                    }
                    .boxed()
                }));
        }
        server
            .add_accessory(fan_acc)
            .await
            .map_err(|e| anyhow::anyhow!("add fan {fan_name}: {e}"))?;

        // -- Lightbulb accessory (only for fans with a physical light) --
        if fan.has_light {
            let mut light_acc = LightbulbAccessory::new(
                light_aid,
                AccessoryInformation {
                    name: format!("{fan_name} light"),
                    manufacturer: "fan-controller".into(),
                    model: "OOK-433".into(),
                    serial_number: format!("{serial}-light"),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("light {fan_name}: {e}"))?;

            let sh = shared.clone();
            let target = fan_name.clone();
            light_acc
                .lightbulb
                .power_state
                .on_update_async(Some(move |_old: bool, new: bool| {
                    let sh = sh.clone();
                    let target = target.clone();
                    async move {
                        // Only transmit when the desired state differs from what
                        // we last sent, since the remote has no absolute set.
                        let toggle = {
                            let mut map = sh.light_on.lock().unwrap();
                            let cur = map.get(&target).copied().unwrap_or(false);
                            if cur != new {
                                map.insert(target.clone(), new);
                                true
                            } else {
                                false
                            }
                        };
                        if toggle {
                            transmit(sh, target, "toggle_light".to_string()).await;
                        }
                        Ok::<(), BoxErr>(())
                    }
                    .boxed()
                }));
            server
                .add_accessory(light_acc)
                .await
                .map_err(|e| anyhow::anyhow!("add light {fan_name}: {e}"))?;
        }
    }

    let pin_str: String = pin.iter().map(|d| d.to_string()).collect();
    eprintln!(
        "[homekit] '{name}' advertising {} fans. Pair in the Home app with code {}-{}-{}",
        shared.config.fans.len(),
        &pin_str[0..3],
        &pin_str[3..5],
        &pin_str[5..8],
    );

    server
        .run_handle()
        .await
        .map_err(|e| anyhow::anyhow!("server: {e}"))
}
