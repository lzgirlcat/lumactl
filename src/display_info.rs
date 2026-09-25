use std::{format, process::Command};

use eyre::{Context, Result};

#[derive(serde::Deserialize)]
pub struct DisplayInfo {
    pub model: String,
    pub name: String,
    pub serial: String,
    pub make: String,
}

impl DisplayInfo {
    pub fn get_displays() -> Result<Vec<Self>> {
        let outputs = String::from_utf8(
            Command::new("swaymsg")
                .args(["-t", "get_outputs", "--raw"])
                .output()?
                .stdout,
        )?;
        serde_json::from_str(&outputs).context("failed to swaymsg output")
    }

    /// Match the display name against the display's model name, id or description
    pub fn match_name(&self, display_name: &str) -> bool {
        self.name.contains(display_name)
            || self.model.contains(display_name)
            || self.serial.contains(display_name)
            || self.make.contains(display_name)
            || self.full_name() == display_name
    }

    pub fn full_name(&self) -> String {
        return format!("{} {} {}", self.make, self.model, self.serial);
    }
}
