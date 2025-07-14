// Fan controller module for managing fan speed and mode based on temperature and configuration.
use std::{io::Write, path::PathBuf};

use crate::{
    config::{FanConfig, SpeedCurve},
    error::{Error, Result},
};

#[derive(Debug)]
// Represents a fan controller that can set fan speed and mode based on configuration.
pub struct FanController {
    // File handle for writing manual mode (on/off)
    manual_file: std::fs::File,
    // File handle for writing fan speed output
    output_file: std::fs::File,
    // Configuration for fan behavior
    config: FanConfig,
    // Minimum allowed fan speed
    min_speed: u32,
    // Maximum allowed fan speed
    max_speed: u32,
}

impl FanController {
    /// Creates a new FanController for the given fan path and configuration.
    ///
    /// Opens the necessary files for manual mode and speed control, and reads min/max speeds.
    pub fn new(path: PathBuf, config: FanConfig) -> Result<Self> {
        // Helper to append a suffix to the file name in the path
        fn join_suffix(mut path: PathBuf, suffix: &str) -> PathBuf {
            let file_name = path.file_name().unwrap().to_str().unwrap();
            path.set_file_name(format!("{file_name}{suffix}"));
            path
        }

        // Read minimum speed from file
        let min_speed = std::fs::read_to_string(join_suffix(path.clone(), "_min"))
            .map_err(Error::MinSpeedRead)?
            .trim()
            .parse()
            .map_err(Error::MinSpeedParse)?;

        // Read maximum speed from file
        let max_speed = std::fs::read_to_string(join_suffix(path.clone(), "_max"))
            .map_err(Error::MaxSpeedRead)?
            .trim_end()
            .parse()
            .map_err(Error::MaxSpeedParse)?;

        // Prepare to open files for writing
        let mut open_options = std::fs::OpenOptions::new();
        open_options.write(true).truncate(true);

        // Open manual mode file
        let manual_file = open_options
            .open(join_suffix(path.clone(), "_manual"))
            .map_err(Error::FanOpen)?;

        // Open output (speed) file
        let output_file = open_options
            .open(join_suffix(path, "_output"))
            .map_err(Error::FanOpen)?;

        let this = Self {
            manual_file,
            output_file,
            config,
            min_speed,
            max_speed,
        };

        // Print debug info about the found fan
        println!("Found fan: {this:#?}");
        Ok(this)
    }

    /// Enable or disable manual mode for the fan.
    /// Writes '1' to enable, '0' to disable.
    pub fn set_manual(&self, enabled: bool) -> Result<()> {
        (&self.manual_file)
            .write_all(if enabled { b"1" } else { b"0" })
            .map_err(Error::FanWrite)
    }

    /// Set the fan speed, clamping to min/max allowed values.
    /// Writes the speed value to the output file.
    pub fn set_speed(&self, mut speed: u32) -> Result<()> {
        // Clamp speed to allowed range
        if speed < self.min_speed {
            speed = self.min_speed;
        } else if speed > self.max_speed {
            speed = self.max_speed;
        }

        // Print the speed being set (overwriting the current line)
        print!("\x1b[1K\rSetting fan speed to {speed}");
        let _ = std::io::stdout().lock().flush();

        // Write the speed to the output file
        write!(&self.output_file, "{speed}").map_err(Error::FanWrite)?;
        Ok(())
    }

    /// Calculate the appropriate fan speed for a given temperature.
    ///
    /// Uses the configured speed curve (linear, exponential, logarithmic) and clamps to min/max.
    pub fn calc_speed(&self, temp: u8) -> u32 {
        // If always_full_speed is set, return max speed
        if self.config.always_full_speed {
            return self.max_speed;
        }

        // If below low_temp, use min speed
        if temp <= self.config.low_temp {
            return self.min_speed;
        }
        // If above high_temp, use max speed
        if temp >= self.config.high_temp {
            return self.max_speed;
        }

        // Interpolate speed based on temperature and curve type
        let temp = temp as u32;
        let low_temp = self.config.low_temp as u32;
        let high_temp = self.config.high_temp as u32;
        match self.config.speed_curve {
            SpeedCurve::Linear => {
                // Linear interpolation between min and max speed
                ((temp - low_temp) as f32 / (high_temp - low_temp) as f32
                    * (self.max_speed - self.min_speed) as f32) as u32
                    + self.min_speed
            }
            SpeedCurve::Exponential => {
                // Exponential curve for more aggressive ramp-up
                ((temp - low_temp).pow(3) as f32 / (high_temp - low_temp).pow(3) as f32
                    * (self.max_speed - self.min_speed) as f32) as u32
                    + self.min_speed
            }
            SpeedCurve::Logarithmic => {
                // Logarithmic curve for gentler ramp-up
                (((temp - low_temp) as f32).log((high_temp - low_temp) as f32)
                    * (self.max_speed - self.min_speed) as f32) as u32
                    + self.min_speed
            }
        }
    }
}
