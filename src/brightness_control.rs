use std::{
    fs, path::{Path, PathBuf},
};

use eyre::{bail, Result};

use crate::{
    backlight::{backlight_brightness, set_backlight_brightness},
    calculate_new_brightness,
    ddc::{ddc_brightness, get_ddc_display, set_ddc_brightness},
    display_info::DisplayInfo,
};

const SYS_DRM_ROOT: &str = "/sys/class/drm/";
const SYS_I2C_DEV_ROOT: &str = "/sys/class/i2c-dev";

pub enum BrightnessControl {
    Backlight(PathBuf),
    I2c(ddc_hi::Display),
}

impl BrightnessControl {
    /// Get the brightness control (either i2c or backlight) from the --display argument
    /// passed by the user, which might me the name, model or description
    pub fn get_from_name(display_arg: &str) -> Result<Self, eyre::Error> {
        let br_ctl = if let Some(br_ctl) = Self::for_device(display_arg) {
            br_ctl
        } else {
            // If we can't find the display by its name, try the model and description
            let displays = DisplayInfo::get_displays()?;
            let display = displays.iter().find(|d| d.match_name(display_arg));
            match display {
                Some(display) => {
                    let br_ctl = BrightnessControl::for_device(&display.name);
                    match br_ctl {
                        Some(br_ctl) => br_ctl,
                        None => bail!("Display {} not found", display.name),
                    }
                }
                None => bail!("Display {} not found", display_arg),
            }
        };
        br_ctl
    }

    pub fn for_device(name: &str) -> Option<Result<Self>> {
        fs::read_dir(SYS_DRM_ROOT)
            .unwrap()
            // Filter the right drm device for the display
            .filter_map(|entry| entry.ok())
            .find_map(|entry| {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name.starts_with("card") && file_name.ends_with(name) {
                    // Try searching for the backlight first
                    if let Some(backlight) = fs::read_dir(entry.path())
                        .unwrap()
                        .filter_map(|entry| entry.ok())
                        .find_map(|entry| {
                            let file_name = entry.file_name();
                            let file_name = file_name.to_string_lossy();
                            ["amdgpu_bl", "intel_backlight", "acpi_video"]
                                .iter()
                                .find_map(|backlight| {
                                    if file_name.starts_with(backlight) {
                                        Some(entry.path())
                                    } else {
                                        None
                                    }
                                })
                        })
                    {
                        return Some(Ok(BrightnessControl::Backlight(backlight)));
                    }
                    // Try all the available i2c devices before the ddc symlink
                    // This works for DP
                    for index in 1..=22 {
                        let i2c_device = format!("i2c-{index}");
                        let path = entry.path().join(&i2c_device);
                        if path.exists() {
                            let ddc_display = get_ddc_display(&i2c_device);
                            match ddc_display {
                                Ok(ddc_display) => {
                                    return Some(Ok(BrightnessControl::I2c(ddc_display)));
                                }
                                Err(err) => {
                                    return Some(Err(err));
                                }
                            }
                        }
                    }
                    // Fallback to the ddc symlink, works for HDMI
                    if let Ok(ddc_path) = entry.path().join("ddc").read_link() {
                        let ddc_path = ddc_path.file_name().unwrap();

                        let ddc_display = get_ddc_display(&ddc_path.to_string_lossy());
                        return match ddc_display {
                            Ok(ddc_display) => Some(Ok(BrightnessControl::I2c(ddc_display))),
                            Err(err) => Some(Err(err)),
                        };
                    }
                    // Fallback for connectors with no ddc symlink and no direct
                    // i2c-N entry (seen on amdgpu/DP setups, and especially
                    // MST topologies where multiple monitors sit behind
                    // generically-named "DPMST" i2c-dev adapters that don't
                    // correspond 1:1 to a connector's drm_dp_auxN index).
                    //
                    // Rather than guess by name/index, we verify by content:
                    // the connector's own EDID is already available at
                    // .../edid via sysfs (populated by the kernel regardless
                    // of MST complexity), so we probe every plausible GPU
                    // aux/i2c adapter, read its EDID, and match it against
                    // that known-good EDID.
                    if let Some(ddc_display) = Self::resolve_i2c_by_edid_match(&entry.path()) {
                        return Some(Ok(BrightnessControl::I2c(ddc_display)));
                    }
                    None
                } else {
                    None
                }
            })
    }

    /// Resolve the i2c-dev adapter for a DRM connector by matching EDIDs,
    /// rather than by name or index. This is the only reliable approach for
    /// MST-connected monitors, where the i2c-dev adapter backing each sink
    /// is typically named generically ("DPMST") and has no direct
    /// correspondence to the connector's drm_dp_auxN directory index.
    fn resolve_i2c_by_edid_match(connector_path: &Path) -> Option<ddc_hi::Display> {
        let expected_edid = fs::read(connector_path.join("edid")).ok()?;
        if expected_edid.is_empty() {
            return None;
        }

        let compare_len = expected_edid.len().min(0x80);

        for i2c_entry in fs::read_dir(SYS_I2C_DEV_ROOT).ok()?.filter_map(|e| e.ok()) {
            let name = fs::read_to_string(i2c_entry.path().join("name")).unwrap_or_default();
            let name = name.trim();

            if !(name.contains("DPMST")) {
                continue;
            }

            let bus_name = i2c_entry.file_name().to_string_lossy().into_owned();
            let Ok(ddc_display) = get_ddc_display(&bus_name) else {
                continue;
            };

            if ddc_display.info.edid_data.clone().unwrap()[..compare_len] == expected_edid[..compare_len]
            {
                return Some(ddc_display);
            }
        }

        None
    }

    pub fn brightness(&mut self) -> Result<(u32, u32)> {
        match self {
            BrightnessControl::Backlight(backlight) => backlight_brightness(Path::new(backlight)),
            BrightnessControl::I2c(ref mut i2c_display) => {
                ddc_brightness(i2c_display).map(|(br, max)| (br as u32, max as u32))
            }
        }
    }

    pub(crate) fn set_brightness(&mut self, new_br: &str) -> Result<()> {
        let current_brightness = self.brightness()?;
        let final_brightness = calculate_new_brightness(current_brightness, new_br)?;

        match self {
            BrightnessControl::Backlight(backlight) => {
                set_backlight_brightness(Path::new(backlight), final_brightness)
            }
            BrightnessControl::I2c(ref mut i2c_display) => {
                set_ddc_brightness(i2c_display, final_brightness.try_into()?)
            }
        }
    }
}
