//! Logical representation of a device: serial, model, state, transport hints.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceState {
    Online,
    Offline,
    Unauthorized,
    Recovery,
    Sideload,
    Bootloader,
    Disconnected,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub state: DeviceState,
    pub model: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub transport_id: Option<u32>,
}

impl DeviceInfo {
    pub fn display_name(&self) -> String {
        self.model
            .clone()
            .or_else(|| self.product.clone())
            .map(|m| format!("{} ({})", m, self.id.0))
            .unwrap_or_else(|| self.id.0.clone())
    }
}
