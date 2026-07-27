use anyhow::{Context, Result, bail};
use evdev::{
    AbsoluteAxisCode, AttributeSet, BusType, Device, InputEvent, InputId, KeyCode,
    RelativeAxisCode, uinput::VirtualDevice,
};
use std::path::PathBuf;
use tracing::info;

use crate::protocol::{AbsAxisWire, DeviceMetadata, EventWire};

/// Type of input device detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Gamepad,
    Joystick,
    Keyboard,
    Mouse,
    Other,
}

impl std::fmt::Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceType::Gamepad => write!(f, "Gamepad"),
            DeviceType::Joystick => write!(f, "Joystick"),
            DeviceType::Keyboard => write!(f, "Keyboard"),
            DeviceType::Mouse => write!(f, "Mouse"),
            DeviceType::Other => write!(f, "Other"),
        }
    }
}

/// Information about a discovered input device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub device_type: DeviceType,
    pub vendor: u16,
    pub product: u16,
}

/// Scanner for listing local input devices.
pub struct DeviceScanner;

impl DeviceScanner {
    /// Check if a device name represents non-controller system noise.
    pub fn is_noise_device(name: &str) -> bool {
        let n = name.to_lowercase();
        n.contains("motion sensors")
            || n.contains("system control")
            || n.contains("consumer control")
            || n.contains("hdmi")
            || n.contains("pcm=")
            || n.contains("headphone")
            || n.contains("video bus")
            || n.contains("lid switch")
            || n.contains("power button")
            || n.contains("sleep button")
            || n.contains("pc speaker")
            || n.contains("hotkeys")
            || n.contains("button array")
            || n.contains("privacy driver")
            || n.contains("wireless controller touchpad")
    }

    /// Enumerate `/dev/input/event*` devices with specific type selection filters.
    pub fn list_devices_filtered(
        include_keyboard: bool,
        include_mouse: bool,
        include_all: bool,
    ) -> Vec<DeviceInfo> {
        let mut devices = Vec::new();
        for (path, device) in evdev::enumerate() {
            let name = device.name().unwrap_or("Unknown Device").to_string();
            let input_id = device.input_id();
            let device_type = Self::classify_device(&device);

            if !include_all {
                if device_type == DeviceType::Other || Self::is_noise_device(&name) {
                    continue;
                }
                match device_type {
                    DeviceType::Gamepad | DeviceType::Joystick => {}
                    DeviceType::Keyboard if include_keyboard => {}
                    DeviceType::Mouse if include_mouse => {}
                    _ => continue,
                }
            }

            devices.push(DeviceInfo {
                path,
                name,
                device_type,
                vendor: input_id.vendor(),
                product: input_id.product(),
            });
        }
        devices.sort_by(|a, b| a.path.cmp(&b.path));
        devices
    }

    /// Enumerate gamepads and joysticks by default.
    pub fn list_devices() -> Vec<DeviceInfo> {
        Self::list_devices_filtered(false, false, false)
    }

    /// Classify device based on its supported keys and axes.
    pub fn classify_device(device: &Device) -> DeviceType {
        let keys = device.supported_keys();
        let abs = device.supported_absolute_axes();
        let rel = device.supported_relative_axes();

        let has_gamepad_btn = keys.is_some_and(|k| {
            k.contains(KeyCode::BTN_SOUTH)
                || k.contains(KeyCode::BTN_EAST)
                || k.contains(KeyCode::BTN_NORTH)
                || k.contains(KeyCode::BTN_WEST)
                || k.contains(KeyCode::BTN_TL)
                || k.contains(KeyCode::BTN_TR)
                || k.contains(KeyCode::BTN_SELECT)
                || k.contains(KeyCode::BTN_START)
                || k.contains(KeyCode::BTN_MODE)
                || k.contains(KeyCode::BTN_THUMBL)
                || k.contains(KeyCode::BTN_THUMBR)
        });

        let has_joystick_btn = keys
            .is_some_and(|k| k.contains(KeyCode::BTN_TRIGGER) || k.contains(KeyCode::BTN_THUMB));

        let has_keyboard_keys =
            keys.is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_ENTER));

        let has_mouse_btn =
            keys.is_some_and(|k| k.contains(KeyCode::BTN_LEFT) && k.contains(KeyCode::BTN_RIGHT));

        if has_gamepad_btn {
            DeviceType::Gamepad
        } else if has_joystick_btn || (abs.is_some() && has_gamepad_btn) {
            DeviceType::Joystick
        } else if has_mouse_btn || rel.is_some() {
            DeviceType::Mouse
        } else if has_keyboard_keys {
            DeviceType::Keyboard
        } else {
            DeviceType::Other
        }
    }

    /// Find the first available gamepad device path.
    pub fn find_first_gamepad() -> Option<PathBuf> {
        Self::list_devices()
            .into_iter()
            .find(|d| d.device_type == DeviceType::Gamepad || d.device_type == DeviceType::Joystick)
            .map(|d| d.path)
    }
}

/// Helper to extract metadata from a physical device.
pub fn extract_metadata(device: &Device) -> DeviceMetadata {
    let name = device
        .name()
        .unwrap_or("Joycast Virtual Device")
        .to_string();
    let id = device.input_id();

    let mut keys = Vec::new();
    if let Some(supported) = device.supported_keys() {
        for k in supported.iter() {
            keys.push(k.0);
        }
    }

    let mut abs_axes = Vec::new();
    if let Some(supported) = device.supported_absolute_axes()
        && let Ok(abs_infos) = device.get_abs_state()
    {
        for (code, info) in abs_infos.iter().enumerate() {
            let code_u16 = code as u16;
            let abs_code = AbsoluteAxisCode(code_u16);
            if supported.contains(abs_code) {
                abs_axes.push(AbsAxisWire {
                    code: code_u16,
                    value: info.value,
                    minimum: info.minimum,
                    maximum: info.maximum,
                    fuzz: info.fuzz,
                    flat: info.flat,
                    resolution: info.resolution,
                });
            }
        }
    }

    let mut rel_axes = Vec::new();
    if let Some(supported) = device.supported_relative_axes() {
        for r in supported.iter() {
            rel_axes.push(r.0);
        }
    }

    DeviceMetadata {
        name,
        bustype: id.bus_type().0,
        vendor: id.vendor(),
        product: id.product(),
        version: id.version(),
        keys,
        abs_axes,
        rel_axes,
    }
}

/// Virtual uinput device created on the server to replay events.
pub struct VirtualOutput {
    name: String,
    device: VirtualDevice,
}

impl VirtualOutput {
    /// Create a new virtual uinput device from client metadata.
    pub fn new(meta: &DeviceMetadata) -> Result<Self> {
        let dev_name = format!("Joycast: {}", meta.name);

        let mut builder = match VirtualDevice::builder() {
            Ok(b) => b,
            Err(e) => bail!(
                "Failed to open /dev/uinput ({}). Run 'sudo joycast server' or add your user to the 'input' group with uinput udev rules enabled.",
                e
            ),
        };

        builder = builder.name(&dev_name).input_id(InputId::new(
            BusType(meta.bustype),
            meta.vendor,
            meta.product,
            meta.version,
        ));

        let mut key_set = AttributeSet::<KeyCode>::new();
        for &k in &meta.keys {
            key_set.insert(KeyCode(k));
        }
        if !meta.keys.is_empty() {
            builder = builder.with_keys(&key_set).context("Failed to set keys")?;
        }

        let mut rel_set = AttributeSet::<RelativeAxisCode>::new();
        for &r in &meta.rel_axes {
            rel_set.insert(RelativeAxisCode(r));
        }
        if !meta.rel_axes.is_empty() {
            builder = builder
                .with_relative_axes(&rel_set)
                .context("Failed to set relative axes")?;
        }

        for abs in &meta.abs_axes {
            let code = AbsoluteAxisCode(abs.code);
            let info = evdev::AbsInfo::new(
                abs.value,
                abs.minimum,
                abs.maximum,
                abs.fuzz,
                abs.flat,
                abs.resolution,
            );
            builder = builder
                .with_absolute_axis(&evdev::UinputAbsSetup::new(code, info))
                .context("Failed to setup absolute axis")?;
        }

        let device = builder.build().context(
            "Failed to build virtual uinput device (check uinput permissions or run with sudo)",
        )?;
        info!(
            name = %dev_name,
            keys_count = meta.keys.len(),
            abs_count = meta.abs_axes.len(),
            "Virtual uinput device created successfully"
        );

        Ok(Self {
            name: dev_name,
            device,
        })
    }

    /// Emit a batch of wire events onto the uinput device.
    pub fn emit(&mut self, events: &[EventWire]) -> Result<()> {
        let evdev_events: Vec<InputEvent> = events
            .iter()
            .map(|e| InputEvent::new(e.type_, e.code, e.value))
            .collect();

        if !evdev_events.is_empty() {
            self.device
                .emit(&evdev_events)
                .context("Failed to emit events to virtual uinput device")?;
        }
        Ok(())
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        info!(name = %self.name, "Virtual uinput device destroyed and unregistered from system");
    }
}
