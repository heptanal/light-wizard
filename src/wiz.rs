use std::{
    collections::{BTreeSet, HashMap},
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use if_addrs::IfAddr;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizLight {
    pub ip: Ipv4Addr,
    pub mac: Option<String>,
}

impl WizLight {
    pub fn display_name(&self) -> String {
        match &self.mac {
            Some(mac) => format!("{} ({mac})", self.ip),
            None => self.ip.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub ip: Ipv4Addr,
    pub params: Map<String, Value>,
}

pub struct WizClient {
    output_socket: UdpSocket,
    port: u16,
    timeout: Duration,
}

impl WizClient {
    pub fn new(port: u16, timeout: Duration) -> Result<Self> {
        let output_socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .context("failed to create WiZ output UDP socket")?;
        output_socket
            .set_nonblocking(true)
            .context("failed to make WiZ output socket non-blocking")?;
        Ok(Self {
            output_socket,
            port,
            timeout,
        })
    }

    pub fn query_pilot(&self, light: &WizLight) -> Result<StateSnapshot> {
        let result = self.request(light.ip, "getPilot", json!({}))?;
        let params = result
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow!("{} returned a non-object pilot state", light.ip))?;
        Ok(StateSnapshot {
            ip: light.ip,
            params,
        })
    }

    pub fn identify(&self, light: &mut WizLight) {
        if light.mac.is_some() {
            return;
        }
        if let Ok(snapshot) = self.query_pilot(light) {
            light.mac = snapshot
                .params
                .get("mac")
                .and_then(Value::as_str)
                .map(normalize_mac);
        }
    }

    pub fn request(&self, ip: Ipv4Addr, method: &str, params: Value) -> Result<Value> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .with_context(|| format!("failed to create request socket for {ip}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(80)))
            .context("failed to configure WiZ request timeout")?;

        let message = serde_json::to_vec(&json!({
            "method": method,
            "params": params,
        }))?;
        let destination = SocketAddr::from((ip, self.port));
        let deadline = Instant::now() + self.timeout;
        let mut next_send = Instant::now();
        let retry_delays = [
            Duration::ZERO,
            Duration::from_millis(120),
            Duration::from_millis(280),
            Duration::from_millis(500),
        ];
        let mut sends = 0usize;
        let mut buffer = [0_u8; 8_192];

        while Instant::now() < deadline {
            let now = Instant::now();
            if sends < retry_delays.len() && now >= next_send {
                socket
                    .send_to(&message, destination)
                    .with_context(|| format!("failed to send {method} to {ip}"))?;
                let delay = retry_delays[sends];
                sends += 1;
                next_send = now + delay.max(Duration::from_millis(80));
            }

            match socket.recv_from(&mut buffer) {
                Ok((length, source)) => {
                    if source.ip() != IpAddr::V4(ip) {
                        continue;
                    }
                    let response: Value = match serde_json::from_slice(&buffer[..length]) {
                        Ok(response) => response,
                        Err(_) => continue,
                    };
                    if response.get("method").and_then(Value::as_str) != Some(method) {
                        continue;
                    }
                    if let Some(error) = response.get("error") {
                        bail!("{ip} rejected {method}: {error}");
                    }
                    return response
                        .get("result")
                        .cloned()
                        .ok_or_else(|| anyhow!("{ip} returned {method} without result or error"));
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed while waiting for {method} from {ip}"));
                }
            }
        }

        bail!("timed out waiting for {method} from {ip}")
    }

    pub fn send_pilot_one(&self, light: &WizLight, rgb: [u8; 3], dimming: u8) -> Result<()> {
        let message = pilot_message(rgb, dimming)?;
        self.send_to_all(std::slice::from_ref(light), &message)
    }

    pub fn send_pulse(&self, lights: &[WizLight], delta: i16, duration_ms: u32) -> Result<()> {
        let message = serde_json::to_vec(&json!({
            "method": "pulse",
            "params": {
                "delta": delta,
                "duration": duration_ms,
            }
        }))?;
        self.send_to_all(lights, &message)
    }

    pub fn restore(&self, snapshot: &StateSnapshot) -> Result<()> {
        let params = restoration_params(&snapshot.params);
        if params.is_empty() {
            bail!("{} did not provide a restorable pilot state", snapshot.ip);
        }
        let message = serde_json::to_vec(&json!({
            "method": "setPilot",
            "params": params,
        }))?;
        // Restoration matters more than an individual visualizer frame. Repeat
        // the idempotent command without waiting for response traffic.
        for attempt in 0..3 {
            self.output_socket
                .send_to(&message, (snapshot.ip, self.port))
                .with_context(|| format!("failed to restore {}", snapshot.ip))?;
            if attempt < 2 {
                thread::sleep(Duration::from_millis(60));
            }
        }
        Ok(())
    }

    fn send_to_all(&self, lights: &[WizLight], message: &[u8]) -> Result<()> {
        for light in lights {
            match self.output_socket.send_to(message, (light.ip, self.port)) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    // A visualizer frame is stale almost immediately. Dropping
                    // it is preferable to blocking the scheduler.
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to send UDP frame to {}", light.ip));
                }
            }
        }
        Ok(())
    }
}

fn pilot_message(rgb: [u8; 3], dimming: u8) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "method": "setPilot",
        "params": {
            "state": true,
            "r": rgb[0],
            "g": rgb[1],
            "b": rgb[2],
            "dimming": dimming,
        },
    }))?)
}

pub fn discover(
    port: u16,
    duration: Duration,
    configured_broadcasts: &[Ipv4Addr],
) -> Result<Vec<WizLight>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("failed to bind WiZ discovery socket")?;
    socket
        .set_broadcast(true)
        .context("failed to enable UDP broadcast")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(80)))
        .context("failed to configure discovery receive timeout")?;

    let mut broadcasts = interface_broadcasts();
    broadcasts.extend(configured_broadcasts.iter().copied());
    broadcasts.insert(Ipv4Addr::BROADCAST);

    let registration = serde_json::to_vec(&json!({
        "method": "registration",
        "params": {
            "phoneMac": "AAAAAAAAAAAA",
            "register": false,
            "phoneIp": "1.2.3.4",
            "id": "1",
        }
    }))?;

    let deadline = Instant::now() + duration;
    let mut next_broadcast = Instant::now();
    let mut found = HashMap::<Ipv4Addr, WizLight>::new();
    let mut buffer = [0_u8; 8_192];

    while Instant::now() < deadline {
        let now = Instant::now();
        if now >= next_broadcast {
            for broadcast in &broadcasts {
                if let Err(error) = socket.send_to(&registration, (*broadcast, port))
                    && error.kind() != ErrorKind::NetworkUnreachable
                {
                    eprintln!("warning: discovery broadcast to {broadcast}:{port} failed: {error}");
                }
            }
            next_broadcast = now + Duration::from_millis(450);
        }

        match socket.recv_from(&mut buffer) {
            Ok((length, source)) => {
                let IpAddr::V4(ip) = source.ip() else {
                    continue;
                };
                let response: Value = match serde_json::from_slice(&buffer[..length]) {
                    Ok(response) => response,
                    Err(_) => continue,
                };
                if response.get("method").and_then(Value::as_str) != Some("registration") {
                    continue;
                }
                let mac = response
                    .pointer("/result/mac")
                    .and_then(Value::as_str)
                    .map(normalize_mac);
                found.insert(ip, WizLight { ip, mac });
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => {
                return Err(error).context("failed while receiving WiZ discovery replies");
            }
        }
    }

    let mut lights: Vec<_> = found.into_values().collect();
    lights.sort_by_key(|light| u32::from(light.ip));
    Ok(lights)
}

fn interface_broadcasts() -> BTreeSet<Ipv4Addr> {
    let mut broadcasts = BTreeSet::new();
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return broadcasts;
    };
    for interface in interfaces {
        let IfAddr::V4(address) = interface.addr else {
            continue;
        };
        if address.ip.is_loopback() || address.ip.is_link_local() {
            continue;
        }
        let broadcast = address
            .broadcast
            .unwrap_or_else(|| directed_broadcast(address.ip, address.netmask));
        broadcasts.insert(broadcast);
    }
    broadcasts
}

fn directed_broadcast(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) | !u32::from(netmask))
}

fn normalize_mac(mac: &str) -> String {
    mac.chars()
        .filter(|character| character.is_ascii_hexdigit())
        .flat_map(char::to_lowercase)
        .collect()
}

fn restoration_params(state: &Map<String, Value>) -> Map<String, Value> {
    let mut params = Map::new();
    copy_fields(state, &mut params, &["state", "dimming"]);

    let scene_id = state.get("sceneId").and_then(Value::as_i64).unwrap_or(0);
    if scene_id > 0 {
        copy_fields(state, &mut params, &["sceneId", "speed"]);
    } else if ["r", "g", "b"].iter().all(|key| state.contains_key(*key)) {
        copy_fields(state, &mut params, &["r", "g", "b"]);
    } else if state.contains_key("temp") {
        copy_fields(state, &mut params, &["temp"]);
    } else {
        copy_fields(state, &mut params, &["c", "w"]);
    }

    copy_fields(
        state,
        &mut params,
        &["ratio", "fanState", "fanMode", "fanSpeed", "fanRevrs"],
    );
    params
}

fn copy_fields(source: &Map<String, Value>, target: &mut Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = source.get(*field) {
            target.insert((*field).to_owned(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_directed_broadcast() {
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(192, 168, 7, 42),
                Ipv4Addr::new(255, 255, 255, 0)
            ),
            Ipv4Addr::new(192, 168, 7, 255)
        );
    }

    #[test]
    fn restores_only_the_active_color_mode() {
        let state = json!({
            "state": true,
            "dimming": 63,
            "sceneId": 0,
            "r": 12,
            "g": 34,
            "b": 56,
            "temp": 2700,
            "mac": "aabbccddeeff",
            "rssi": -44
        });
        let params = restoration_params(state.as_object().unwrap());
        assert_eq!(params.get("r"), Some(&json!(12)));
        assert!(!params.contains_key("temp"));
        assert!(!params.contains_key("mac"));
    }

    #[test]
    fn prefers_a_running_scene() {
        let state = json!({
            "state": true,
            "dimming": 80,
            "sceneId": 4,
            "speed": 120,
            "r": 255,
            "g": 0,
            "b": 0
        });
        let params = restoration_params(state.as_object().unwrap());
        assert_eq!(params.get("sceneId"), Some(&json!(4)));
        assert!(!params.contains_key("r"));
    }

    #[test]
    fn pilot_payload_keeps_the_light_on_and_sets_color() {
        let payload: Value =
            serde_json::from_slice(&pilot_message([255, 0, 68], 75).unwrap()).unwrap();
        assert_eq!(payload["method"], "setPilot");
        assert_eq!(payload["params"]["state"], true);
        assert_eq!(payload["params"]["r"], 255);
        assert_eq!(payload["params"]["g"], 0);
        assert_eq!(payload["params"]["b"], 68);
        assert_eq!(payload["params"]["dimming"], 75);
    }
}
