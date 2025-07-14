#![warn(rust_2018_idioms)]
#![warn(clippy::pedantic)]
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::similar_names,
    clippy::module_name_repetitions
)]

use std::{
    io::{ErrorKind, Read, Seek},
    path::PathBuf,
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    thread,
    time::Duration,
};

use arraydeque::ArrayDeque;
use fan_controller::FanController;
use nonempty::NonEmpty as NonEmptyVec;
use signal_hook::consts::{SIGINT, SIGTERM};

use config::load_fan_configs;
use error::{Error, Result};

mod config;
mod error;
mod fan_controller;

#[cfg(not(target_os = "linux"))]
compile_error!("This tool is only developed for Linux systems.");

#[cfg(debug_assertions)]
const PID_FILE: &str = "t2fand.pid";
#[cfg(not(debug_assertions))]
const PID_FILE: &str = "/run/t2fand.pid";

// Constants for better readability
const TEMP_BUFFER_SIZE: usize = 50;
const STABLE_TEMP_SLEEP_DURATION: Duration = Duration::from_secs(1);
const RESPONSIVE_SLEEP_DURATION: Duration = Duration::from_millis(100);
const STABLE_TEMP_PADDING_COUNT: usize = 9;

/// Represents the current temperature state and provides methods for temperature management
struct TemperatureMonitor {
    temp_buffer: String,
    cpu_temp_file: std::fs::File,
    gpu_temp_file: Option<std::fs::File>,
    temperature_history: ArrayDeque<u8, TEMP_BUFFER_SIZE, arraydeque::Wrapping>,
    last_mean_temp: u16,
}

impl TemperatureMonitor {
    fn new(
        temp_buffer: String,
        cpu_temp_file: std::fs::File,
        gpu_temp_file: Option<std::fs::File>,
    ) -> Self {
        Self {
            temp_buffer,
            cpu_temp_file,
            gpu_temp_file,
            temperature_history: ArrayDeque::new(),
            last_mean_temp: 0,
        }
    }

    /// Reads current temperatures and returns the higher of CPU/GPU
    fn read_current_temperature(&mut self) -> Result<u8> {
        let cpu_temp = read_temp_file(&mut self.cpu_temp_file, &mut self.temp_buffer)?;
        
        let current_temp = if let Some(gpu_temp_file) = &mut self.gpu_temp_file {
            let gpu_temp = read_temp_file(gpu_temp_file, &mut self.temp_buffer)?;
            cpu_temp.max(gpu_temp)
        } else {
            cpu_temp
        };
        
        Ok(current_temp)
    }

    /// Adds a temperature reading to the history and calculates the mean
    fn update_temperature_history(&mut self, temp: u8) -> u16 {
        self.temperature_history.push_back(temp);
        
        let sum: u16 = self.temperature_history.iter().map(|&t| t as u16).sum();
        sum / (self.temperature_history.len() as u16)
    }

    /// Checks if the temperature has stabilized (no change from last reading)
    fn is_temperature_stable(&self, current_mean: u16) -> bool {
        current_mean == self.last_mean_temp
    }

    /// Pads the temperature history when temperature is stable to maintain accuracy during longer sleep
    fn pad_temperature_history_for_stable_period(&mut self, temp: u8) {
        for _ in 0..STABLE_TEMP_PADDING_COUNT {
            self.temperature_history.push_back(temp);
        }
    }

    /// Updates the last known mean temperature
    fn update_last_mean_temp(&mut self, mean_temp: u16) {
        self.last_mean_temp = mean_temp;
    }
}

fn get_current_euid() -> libc::uid_t {
    // SAFETY: FFI call with no preconditions
    unsafe { libc::geteuid() }
}

fn find_fan_paths() -> Result<NonEmptyVec<PathBuf>> {
    // APP0001:00/fan1_label
    let fan = glob::glob("/sys/devices/pci*/*/*/*/APP0001:00/fan*")?
        .filter_map(Result::ok)
        .find(|p| p.exists())
        .ok_or(Error::NoFan)?;

    // APP0001:00
    let first_fan_path = fan.parent().ok_or(Error::NoFan)?;
    // APP0001:00/fan*_input
    let fan_glob = first_fan_path.display().to_string() + "/fan*_input";
    // APP0001:00/fan1
    let fans = glob::glob(&fan_glob)?
        .filter_map(Result::ok)
        .filter_map(|mut path| {
            let file_name = path.file_name()?.to_str()?;
            let fan_name = file_name.strip_suffix("_input")?;
            let fan_name_owned = fan_name.to_owned();
            path.set_file_name(fan_name_owned);
            Some(path)
        });

    NonEmptyVec::collect(fans).ok_or(Error::NoFan)
}

fn check_pid_file() -> Result<()> {
    match std::fs::read_to_string(PID_FILE) {
        Ok(pid) => {
            let mut proc_path = std::path::PathBuf::new();
            proc_path.push("/proc");
            proc_path.push(pid);

            if proc_path.exists() {
                return Err(Error::AlreadyRunning);
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(Error::PidRead(err)),
    };

    let current_pid = std::process::id().to_string();
    std::fs::write(PID_FILE, current_pid).map_err(Error::PidWrite)
}

fn read_temp_file(temp_file: &mut std::fs::File, temp_buf: &mut String) -> Result<u8> {
    temp_file
        .read_to_string(temp_buf)
        .map_err(Error::TempRead)?;

    temp_file.rewind().map_err(Error::TempSeek)?;

    let temp = temp_buf.trim_end().parse::<u32>().map_err(Error::TempParse);
    temp_buf.clear();
    temp.map(|t| (t / 1000) as u8)
}

fn find_temp_file(temps: glob::Paths, temp_buf: &mut String) -> Option<std::fs::File> {
    for temp_path_res in temps {
        let Ok(temp_path) = temp_path_res else {
            eprintln!("Unable to read glob path");
            continue;
        };

        let Ok(mut temp_file) = std::fs::File::open(temp_path) else {
            eprintln!("Unable to open temperature sensor");
            continue;
        };

        if read_temp_file(&mut temp_file, temp_buf).is_ok() {
            return Some(temp_file);
        }
    }

    None
}

fn find_cpu_temp_file(temp_buf: &mut String) -> Result<std::fs::File> {
    let temps = glob::glob("/sys/devices/platform/coretemp.0/hwmon/hwmon*/temp1_input")?;
    find_temp_file(temps, temp_buf).ok_or(Error::NoCpu)
}

fn find_gpu_temp_file(temp_buf: &mut String) -> Result<Option<std::fs::File>> {
    let temps = glob::glob("/sys/class/drm/card0/device/hwmon/hwmon*/temp1_input")?;
    Ok(find_temp_file(temps, temp_buf))
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Updates all fan speeds based on the current temperature
fn update_fan_speeds(fans: &NonEmptyVec<FanController>, temperature: u8) -> Result<()> {
    for fan in fans {
        let target_speed = fan.calc_speed(temperature);
        fan.set_speed(target_speed)?;
    }
    Ok(())
}

/// Handles the sleep logic based on whether temperature has changed
fn handle_sleep_cycle(
    temperature_monitor: &mut TemperatureMonitor,
    current_temp: u8,
    mean_temp: u16,
    is_stable: bool,
) {
    if is_stable {
        // Temperature is stable - use longer sleep but pad history to maintain accuracy
        temperature_monitor.pad_temperature_history_for_stable_period(current_temp);
        thread::sleep(STABLE_TEMP_SLEEP_DURATION);
    } else {
        // Temperature changed - use shorter sleep for responsiveness
        temperature_monitor.update_last_mean_temp(mean_temp);
        thread::sleep(RESPONSIVE_SLEEP_DURATION);
    }
}

fn start_temp_loop(
    temp_buffer: String,
    cpu_temp_file: std::fs::File,
    gpu_temp_file: Option<std::fs::File>,
    fans: &NonEmptyVec<FanController>,
) -> Result<()> {
    let cancellation_token = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, cancellation_token.clone()).map_err(Error::Signal)?;
    signal_hook::flag::register(SIGTERM, cancellation_token.clone()).map_err(Error::Signal)?;

    let mut temperature_monitor = TemperatureMonitor::new(temp_buffer, cpu_temp_file, gpu_temp_file);

    while !cancellation_token.load(Ordering::Relaxed) {
        // Read current temperature from sensors
        let current_temp = temperature_monitor.read_current_temperature()?;
        
        // Update temperature history and calculate mean
        let mean_temp = temperature_monitor.update_temperature_history(current_temp);
        
        // Check if temperature has stabilized
        let is_stable = temperature_monitor.is_temperature_stable(mean_temp);
        
        // Update fan speeds only if temperature has changed
        if !is_stable {
            update_fan_speeds(fans, mean_temp as u8)?;
        }
        
        // Handle sleep cycle based on temperature stability
        handle_sleep_cycle(&mut temperature_monitor, current_temp, mean_temp, is_stable);
    }

    Ok(())
}

fn real_main() -> Result<()> {
    if get_current_euid() != 0 {
        return Err(Error::NotRoot);
    }

    check_pid_file()?;

    let mut temp_buffer = String::new();

    let fan_paths = find_fan_paths()?;
    let fans = load_fan_configs(fan_paths)?;
    let cpu_temp_file = find_cpu_temp_file(&mut temp_buffer)?;
    let gpu_temp_file = find_gpu_temp_file(&mut temp_buffer)?;

    println!();
    for fan in &fans {
        fan.set_manual(true)?;
    }

    let res = start_temp_loop(temp_buffer, cpu_temp_file, gpu_temp_file, &fans);
    println!("T2 Fan Daemon is shutting down...");
    for fan in fans {
        fan.set_manual(false)?;
    }

    let pid_res = std::fs::remove_file(PID_FILE).map_err(Error::PidDelete);
    match (res, pid_res) {
        (Err(err), _) | (_, Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}
