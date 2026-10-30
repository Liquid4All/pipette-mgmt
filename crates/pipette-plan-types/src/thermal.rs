//! Per-platform thermal telemetry, verbatim to each vendor's API: collapsing
//! Android's seven levels onto Apple's four would lose information. The enums
//! are declared coolest→hottest and derive `Ord` on that order.

use serde::{Deserialize, Serialize};

/// Run-environment power state at benchmark time, reported alongside each
/// result. Plugged in but holding lifts the battery current-limit that can
/// throttle the SoC, as charging does, unlike running on battery.
///
/// `None` / absent on the wire means the client didn't report it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DevicePowerState {
    /// On external power and the battery is charging.
    Charging,
    /// Running on battery (unplugged), discharging.
    NotCharging,
    /// On external power but not adding charge (battery full or
    /// charge-limited / maintenance), or a battery-less desktop on AC.
    PluggedInNotCharging,
}

impl DevicePowerState {
    /// The snake_case wire string, identical to the serde form.
    pub fn as_wire_str(self) -> &'static str {
        self.into()
    }
}

/// Apple `ProcessInfo.thermalState` (iOS / iPadOS / macOS). Absent on the wire
/// when unreported or off Apple platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppleThermalState {
    /// No thermal pressure; the device is operating normally.
    Nominal,
    /// Mild thermal pressure; fans may spin up but performance is unaffected.
    Fair,
    /// The system is actively shedding heat and may be throttling.
    Serious,
    /// Severe thermal pressure; aggressive throttling to prevent damage.
    Critical,
}

/// Android `PowerManager.getCurrentThermalStatus()` `THERMAL_STATUS_*` levels.
/// Distinct from [`AndroidThrottlingSeverity`]: same level names, separate API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidThermalStatus {
    /// No throttling. Serializes to `"none"`.
    None,
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
    Shutdown,
}

/// Android thermal-HAL per-sensor `ThrottlingSeverity`. Distinct from the
/// device-level [`AndroidThermalStatus`]: same level names, separate API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidThrottlingSeverity {
    /// No throttling. Serializes to `"none"`.
    None,
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
    Shutdown,
}

/// One Android thermal-HAL `Temperature` reading for one sensor at one measured
/// repetition. `celsius` is whole °C, rounded by the client from the HAL float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AndroidTemperatureSensor {
    /// Zero-based index of the measured repetition this reading was sampled at.
    pub iteration: i32,
    /// A snake_case `TemperatureType` name, e.g. `cpu` / `skin`.
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub name: String,
    pub celsius: i32,
    pub throttling_status: AndroidThrottlingSeverity,
}

/// One Linux thermal-zone reading at one measured repetition. `celsius` is
/// whole °C, converted by the client from sysfs milli-°C.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinuxThermalZone {
    /// Zero-based index of the measured repetition this reading was sampled at.
    pub iteration: i32,
    /// The sysfs zone `type`, e.g. `x86_pkg_temp` / `cpu-thermal`.
    #[serde(rename = "type")]
    pub zone_type: String,
    pub celsius: i32,
}

// Per-run series; the sampling lives with the prober (`pipette_device`).

/// One thermal snapshot for the current platform. Every field is `Option`;
/// only the running OS's thermal family is populated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThermalReading {
    pub apple_thermal_state: Option<AppleThermalState>,
    /// Apple SoC die temperature in whole °C, the hottest private IOHID
    /// `PMU tdie*` sensor, by the same rule on iOS (`PIPETTE_PRIVATE_THERMAL`
    /// builds) and macOS. `None` elsewhere or where the symbols don't resolve.
    pub apple_soc_temp_c: Option<f32>,
    pub android_thermal_status: Option<AndroidThermalStatus>,
    pub android_thermal_headroom: Option<f32>,
    pub android_thermal_sensors: Option<Vec<AndroidTemperatureSensor>>,
    pub linux_thermal_zones: Option<Vec<LinuxThermalZone>>,
}

/// Per-iteration thermal snapshots bracketing each measured repetition:
/// `before[i]` at rep `i`'s gate-pass, `after[i]` once its timed work completes.
///
/// Empty by design for runners that don't gate (eval, max-memory): neither a
/// score nor a peak byte count moves with temperature, so it is not a gap to fill.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunThermal {
    pub before: Vec<ThermalReading>,
    pub after: Vec<ThermalReading>,
}

impl RunThermal {
    /// Build from per-repetition `(before, after)` snapshot pairs, in rep order.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (ThermalReading, ThermalReading)>) -> Self {
        let (before, after) = pairs.into_iter().unzip();
        RunThermal { before, after }
    }
}

/// Per-platform, per-iteration thermal telemetry, flattened into
/// `BenchmarkSubmissionPayload`. Each family is set only by the platform that
/// exposes it; the rest stay `None`.
///
/// `_before` is sampled at each measured repetition's gate-pass, `_after` once
/// its timed work completes. Scalar families carry one value per repetition;
/// sensor/zone families are one list, each element tagged with its `iteration`.
///
/// Lives here, not with the payload: the Android JNI bridge builds it and never
/// links the submission path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThermalTelemetry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_apple_thermal_state_before: Option<Vec<AppleThermalState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_apple_thermal_state_after: Option<Vec<AppleThermalState>>,
    /// Apple SoC die temperature in whole °C, one reading per rep in rep order,
    /// from the private IOHID `PMU tdie*` sensors on iOS (`PIPETTE_PRIVATE_THERMAL`
    /// builds) and macOS. Sampled with `device_apple_thermal_state_*` but *not*
    /// element-aligned with it: that series drops a rep whose token is unknown.
    /// `None` when unavailable.
    ///
    /// Whole degrees because the column stays `f32`: only integers survive a
    /// reader widening it to 64-bit. On macOS it is recorded but not gated on:
    /// idle die temperature spans ~38–53 °C across the fleet, so no constant
    /// threshold fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_apple_soc_temp_c_before: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_apple_soc_temp_c_after: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_android_thermal_status_before: Option<Vec<AndroidThermalStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_android_thermal_status_after: Option<Vec<AndroidThermalStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_android_thermal_headroom_before: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_android_thermal_headroom_after: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_android_thermal_sensors_before: Option<Vec<AndroidTemperatureSensor>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_android_thermal_sensors_after: Option<Vec<AndroidTemperatureSensor>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_linux_thermal_zones_before: Option<Vec<LinuxThermalZone>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_linux_thermal_zones_after: Option<Vec<LinuxThermalZone>>,
}

impl ThermalTelemetry {
    /// Build the wire fields from the per-repetition snapshot series.
    pub fn from_series(before: &[ThermalReading], after: &[ThermalReading]) -> Self {
        ThermalTelemetry {
            device_apple_thermal_state_before: scalar_series(before, |r| r.apple_thermal_state),
            device_apple_thermal_state_after: scalar_series(after, |r| r.apple_thermal_state),
            device_apple_soc_temp_c_before: scalar_series(before, |r| r.apple_soc_temp_c),
            device_apple_soc_temp_c_after: scalar_series(after, |r| r.apple_soc_temp_c),
            device_android_thermal_status_before: scalar_series(before, |r| {
                r.android_thermal_status
            }),
            device_android_thermal_status_after: scalar_series(after, |r| r.android_thermal_status),
            device_android_thermal_headroom_before: scalar_series(before, |r| {
                r.android_thermal_headroom
            }),
            device_android_thermal_headroom_after: scalar_series(after, |r| {
                r.android_thermal_headroom
            }),
            device_android_thermal_sensors_before: sensor_series(before),
            device_android_thermal_sensors_after: sensor_series(after),
            device_linux_thermal_zones_before: zone_series(before),
            device_linux_thermal_zones_after: zone_series(after),
        }
    }
}

/// One value per rep, all-or-nothing: the array index is the iteration, so a
/// family missing on one rep (e.g. a `getThermalHeadroom` `NaN`) would shift
/// later values onto the wrong iteration.
fn scalar_series<T>(
    reps: &[ThermalReading],
    get: impl Fn(&ThermalReading) -> Option<T>,
) -> Option<Vec<T>> {
    let values: Vec<T> = reps.iter().filter_map(get).collect();
    (!values.is_empty() && values.len() == reps.len()).then_some(values)
}

/// Flatten the per-rep sensor lists into one list, stamping each element's
/// `iteration` with the rep it came from. `None` when no rep reported sensors.
fn sensor_series(reps: &[ThermalReading]) -> Option<Vec<AndroidTemperatureSensor>> {
    let flat: Vec<AndroidTemperatureSensor> = reps
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            r.android_thermal_sensors
                .iter()
                .flatten()
                .map(move |s| AndroidTemperatureSensor {
                    iteration: i32::try_from(i).unwrap_or(i32::MAX),
                    ..s.clone()
                })
        })
        .collect();
    (!flat.is_empty()).then_some(flat)
}

/// Flatten the per-rep zone lists into one list, stamping each element's
/// `iteration`. See [`sensor_series`].
fn zone_series(reps: &[ThermalReading]) -> Option<Vec<LinuxThermalZone>> {
    let flat: Vec<LinuxThermalZone> = reps
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            r.linux_thermal_zones
                .iter()
                .flatten()
                .map(move |z| LinuxThermalZone {
                    iteration: i32::try_from(i).unwrap_or(i32::MAX),
                    ..z.clone()
                })
        })
        .collect();
    (!flat.is_empty()).then_some(flat)
}

/// Volatile run-environment power state captured per benchmark submission.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PowerReading {
    /// Battery charge percent (0–100); `None` on a device with no battery.
    pub battery_level: Option<i32>,
    /// `None` only when detection failed; a battery-less desktop reports
    /// `PluggedInNotCharging`.
    pub power_state: Option<DevicePowerState>,
    /// OS low-power / battery-saver mode active; `None` where undetectable.
    pub power_save_mode: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_enum_wire_forms_match_server() -> anyhow::Result<()> {
        // Exact snake_case strings the management server's warehouse enums
        // accept. `none` (not `null`) and the coolest→hottest ordering matter.
        assert_eq!(
            serde_json::to_string(&AppleThermalState::Serious)?,
            "\"serious\""
        );
        assert_eq!(
            serde_json::to_string(&AndroidThermalStatus::None)?,
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&AndroidThrottlingSeverity::Shutdown)?,
            "\"shutdown\""
        );
        // `Ord` follows declaration order (coolest→hottest).
        assert!(AppleThermalState::Critical > AppleThermalState::Nominal);
        assert!(AndroidThermalStatus::Severe > AndroidThermalStatus::Light);
        Ok(())
    }

    #[test]
    fn thermal_sensor_element_wire_shape() -> anyhow::Result<()> {
        // The element field names are the Parquet `List<Struct>` child names.
        let sensor = AndroidTemperatureSensor {
            iteration: 2,
            sensor_type: "cpu".to_string(),
            name: "cpu-0-0-usr".to_string(),
            celsius: 61,
            throttling_status: AndroidThrottlingSeverity::Light,
        };
        let value = serde_json::to_value(&sensor)?;
        assert_eq!(value["iteration"], 2);
        assert_eq!(value["type"], "cpu");
        assert_eq!(value["celsius"], 61);
        assert_eq!(value["throttling_status"], "light");
        assert!(value.get("sensor_type").is_none());
        assert_eq!(
            serde_json::from_value::<AndroidTemperatureSensor>(value)?,
            sensor
        );

        let zone = LinuxThermalZone {
            iteration: 0,
            zone_type: "x86_pkg_temp".to_string(),
            celsius: 44,
        };
        let value = serde_json::to_value(&zone)?;
        assert_eq!(value["type"], "x86_pkg_temp");
        assert_eq!(value["celsius"], 44);
        assert_eq!(serde_json::from_value::<LinuxThermalZone>(value)?, zone);
        Ok(())
    }

    #[test]
    fn device_power_state_wire_form_matches_server() -> anyhow::Result<()> {
        // The server's `DevicePowerState` form: serde, `as_wire_str` and
        // `Display` must agree, or submissions get silently dropped.
        for (state, wire) in [
            (DevicePowerState::Charging, "charging"),
            (DevicePowerState::NotCharging, "not_charging"),
            (
                DevicePowerState::PluggedInNotCharging,
                "plugged_in_not_charging",
            ),
        ] {
            assert_eq!(serde_json::to_string(&state)?, format!("\"{wire}\""));
            assert_eq!(state.as_wire_str(), wire);
            assert_eq!(state.to_string(), wire);
        }
        Ok(())
    }

    #[test]
    fn scalar_series_is_all_or_nothing() {
        let all_present = ThermalTelemetry::from_series(
            &[
                ThermalReading {
                    android_thermal_headroom: Some(0.3),
                    ..Default::default()
                },
                ThermalReading {
                    android_thermal_headroom: Some(0.5),
                    ..Default::default()
                },
            ],
            &[],
        );
        assert_eq!(
            all_present.device_android_thermal_headroom_before,
            Some(vec![0.3, 0.5])
        );

        let one_missing = ThermalTelemetry::from_series(
            &[
                ThermalReading {
                    android_thermal_headroom: Some(0.3),
                    ..Default::default()
                },
                // e.g. this rep's `getThermalHeadroom` returned NaN → None
                ThermalReading::default(),
            ],
            &[],
        );
        assert_eq!(one_missing.device_android_thermal_headroom_before, None);
    }

    #[test]
    fn apple_soc_temp_series_reaches_the_wire_fields() -> anyhow::Result<()> {
        let rep = |celsius: f32| ThermalReading {
            apple_thermal_state: Some(AppleThermalState::Nominal),
            apple_soc_temp_c: Some(celsius),
            ..Default::default()
        };
        let telemetry =
            ThermalTelemetry::from_series(&[rep(41.5), rep(39.0)], &[rep(58.25), rep(60.5)]);
        assert_eq!(
            telemetry.device_apple_soc_temp_c_before,
            Some(vec![41.5, 39.0])
        );
        assert_eq!(
            telemetry.device_apple_soc_temp_c_after,
            Some(vec![58.25, 60.5])
        );
        // Flattened at the top level under the warehouse's column name.
        let json = serde_json::to_value(&telemetry)?;
        assert_eq!(json["device_apple_soc_temp_c_before"][0], 41.5);
        assert_eq!(json["device_apple_soc_temp_c_after"][1], 60.5);
        Ok(())
    }

    /// Sensor and zone lists carry their rep as `iteration`, stamped when the
    /// series is flattened, whatever the reading held.
    #[test]
    fn sensor_and_zone_lists_are_stamped_with_their_rep() {
        let rep = |celsius: i32| ThermalReading {
            android_thermal_sensors: Some(vec![AndroidTemperatureSensor {
                iteration: 0,
                sensor_type: "cpu".to_owned(),
                name: "cpu0".to_owned(),
                celsius,
                throttling_status: AndroidThrottlingSeverity::None,
            }]),
            linux_thermal_zones: Some(vec![LinuxThermalZone {
                iteration: 0,
                zone_type: "x86_pkg_temp".to_owned(),
                celsius,
            }]),
            ..Default::default()
        };
        let telemetry = ThermalTelemetry::from_series(&[rep(40), rep(41)], &[]);
        let sensors: Vec<(i32, i32)> = telemetry
            .device_android_thermal_sensors_before
            .iter()
            .flatten()
            .map(|s| (s.iteration, s.celsius))
            .collect();
        assert_eq!(sensors, [(0, 40), (1, 41)]);
        let zones: Vec<(i32, i32)> = telemetry
            .device_linux_thermal_zones_before
            .iter()
            .flatten()
            .map(|z| (z.iteration, z.celsius))
            .collect();
        assert_eq!(zones, [(0, 40), (1, 41)]);
        assert_eq!(telemetry.device_android_thermal_sensors_after, None);
        assert_eq!(telemetry.device_linux_thermal_zones_after, None);
    }

    #[test]
    fn run_thermal_splits_pairs_in_rep_order() {
        let at = |celsius: f32| ThermalReading {
            apple_soc_temp_c: Some(celsius),
            ..Default::default()
        };
        let thermal = RunThermal::from_pairs([(at(40.0), at(50.0)), (at(41.0), at(51.0))]);
        assert_eq!(thermal.before, [at(40.0), at(41.0)]);
        assert_eq!(thermal.after, [at(50.0), at(51.0)]);
    }

    /// Unknown until detected: a zero battery or a `false` saver mode would read
    /// as a measurement.
    #[test]
    fn a_default_power_state_reports_nothing() {
        assert_eq!(
            PowerReading::default(),
            PowerReading {
                battery_level: None,
                power_state: None,
                power_save_mode: None,
            }
        );
    }

    #[test]
    fn partial_apple_soc_temp_suppresses_the_series() {
        let with_temp = ThermalReading {
            apple_soc_temp_c: Some(44.0),
            ..Default::default()
        };
        let telemetry =
            ThermalTelemetry::from_series(&[with_temp.clone(), ThermalReading::default()], &[]);
        assert_eq!(telemetry.device_apple_soc_temp_c_before, None);
    }
}
