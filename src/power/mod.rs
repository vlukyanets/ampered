//! Power source and power modes.
//!
//! Reference: `docs/07-power-supply.md`, `docs/08-power-modes.md`.

/// What a single read of `/sys/class/power_supply` yields.
///
/// "Low battery" is not part of it: that flag carries hysteresis and therefore
/// belongs to the engine, which knows the previous value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PowerSnapshot {
    pub ac: bool,
    /// `None` on a machine with no battery at all.
    pub battery: Option<u8>,
    /// Names of the batteries the percentage was averaged over.
    pub batteries: Vec<String>,
}

impl PowerSnapshot {
    /// A snapshot for a machine we have not looked at yet.
    pub fn on_ac() -> PowerSnapshot {
        PowerSnapshot {
            ac: true,
            battery: None,
            batteries: Vec::new(),
        }
    }
}

/// Anything that can report the current power situation (`docs/14-code-layout.md`).
pub trait PowerSource {
    fn snapshot(&self) -> std::io::Result<PowerSnapshot>;
}
