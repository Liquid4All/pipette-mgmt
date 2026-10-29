//! What the host *is*: the identity fields every benchmark submission
//! carries, its battery reading, and the capability flags a scheduler variant
//! requires of it.

use nutype::nutype;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString};

use crate::TrimmedString;

/// Physical form factor of the device running the benchmark.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, EnumString, EnumIter,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum DeviceFormFactor {
    Phone,
    Tablet,
    Laptop,
    Desktop,
    Server,
    Embedded,
}

impl Default for DeviceFormFactor {
    /// The fallback for old payloads that lack a `device_form_factor`.
    fn default() -> Self {
        Self::Desktop
    }
}

/// Device metadata sent alongside every benchmark submission. `device_name` and
/// `device_form_factor` come from registration; the rest is auto-detected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    #[serde(default)]
    pub device_name: TrimmedString,
    #[serde(default)]
    pub device_form_factor: DeviceFormFactor,
    #[serde(default)]
    pub device_os_name: TrimmedString,
    #[serde(default)]
    pub device_os_version: TrimmedString,
    /// Precise OS build id, finer-grained than `device_os_version` (e.g. macOS
    /// `24F74`, Windows `26100.1234`, Linux kernel release).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_build: Option<TrimmedString>,
    /// OS security-patch level where the platform exposes one (Android).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_security_patch: Option<TrimmedString>,
    /// The CPU architecture that the device reports. The server maps known
    /// aliases to a canonical value for the `arch:` flag, for example `amd64`
    /// to `x86_64` and `aarch64` to `arm64`. It keeps other values as a
    /// lowercase slug. These examples are not the full list:
    /// `canonical_cpu_arch` in `pipette-core` holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_cpu_arch: Option<TrimmedString>,
    /// The OS family that the device reports. The server maps known aliases
    /// to a canonical value for the `os_family:` flag, for example `win32` to
    /// `windows` and `ipados` to `ios`. It keeps other values as a lowercase
    /// slug. These examples are not the full list: `canonical_os_family` in
    /// `pipette-core` holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_family: Option<TrimmedString>,
    #[serde(default)]
    pub device_chip_model: TrimmedString,
    #[serde(default)]
    pub device_ram_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_gpu_model: Option<TrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_gpu_vram_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_npu_model: Option<TrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_npu_vram_bytes: Option<u64>,
}

/// A capability flag a scheduler-mode variant lists in `requires`, such as
/// `os:ios` or `ram_bytes:17179869184`: non-empty, no whitespace, no uppercase,
/// since the server compares flags in that normalized form.
#[nutype(
    validate(predicate = is_canonical_capability_flag),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct CapabilityFlag(String);

/// A predicate, not a regex, so "already lowercase" covers non-ASCII as the
/// server's normalization does.
fn is_canonical_capability_flag(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_whitespace) && s.to_lowercase() == s
}

/// Battery charge as an integer percentage in `0..=100`. A `with` validator so
/// the error names the field and the value sent.
#[nutype::nutype(
    validate(with = validate_battery_level, error = BatteryLevelError),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize,
    ),
)]
pub struct BatteryLevel(i32);

/// A [`BatteryLevel`] outside `0..=100`.
#[derive(Debug, thiserror::Error)]
#[error("device_battery_level ({0}) must be between 0 and 100")]
pub struct BatteryLevelError(pub i32);

fn validate_battery_level(level: &i32) -> Result<(), BatteryLevelError> {
    if (0..=100).contains(level) {
        Ok(())
    } else {
        Err(BatteryLevelError(*level))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[test]
    fn device_info_round_trip_trims_padded_values_via_deserialize() -> anyhow::Result<()> {
        let value = serde_json::json!({
            "device_name": "   Test Device  ",
            "device_form_factor": "desktop",
            "device_os_name": "Windows",
            "device_os_version": "10.0.26100",
            "device_chip_model": "AMD Ryzen AI Max+ 395",
            "device_ram_bytes": 36_000_000_000u64,
            "device_gpu_model": "AMD RYZEN AI MAX+ 395 w/ Radeon 8060S          ",
        });
        let info: DeviceInfo = serde_json::from_value(value)?;
        assert_eq!(info.device_name.as_ref(), "Test Device");
        assert_eq!(
            info.device_gpu_model
                .as_ref()
                .context("device_gpu_model")?
                .as_ref(),
            "AMD RYZEN AI MAX+ 395 w/ Radeon 8060S"
        );
        Ok(())
    }

    #[rstest]
    #[case(DeviceFormFactor::Phone, "phone")]
    #[case(DeviceFormFactor::Tablet, "tablet")]
    #[case(DeviceFormFactor::Laptop, "laptop")]
    #[case(DeviceFormFactor::Desktop, "desktop")]
    #[case(DeviceFormFactor::Server, "server")]
    #[case(DeviceFormFactor::Embedded, "embedded")]
    fn device_form_factor_round_trip(
        #[case] variant: DeviceFormFactor,
        #[case] label: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(variant.to_string(), label);
        assert_eq!(label.parse::<DeviceFormFactor>()?, variant);
        let json = serde_json::to_value(variant)?;
        assert_eq!(json, serde_json::Value::String(label.to_string()));
        assert_eq!(serde_json::from_value::<DeviceFormFactor>(json)?, variant);
        Ok(())
    }

    /// A payload written before a field existed still reads, and an unstated
    /// form factor falls back to `desktop`.
    #[test]
    fn a_device_info_with_no_fields_reads_as_the_defaults() -> anyhow::Result<()> {
        let info: DeviceInfo = serde_json::from_value(serde_json::json!({}))?;
        assert_eq!(
            info,
            DeviceInfo {
                device_name: "".into(),
                device_form_factor: DeviceFormFactor::Desktop,
                device_os_name: "".into(),
                device_os_version: "".into(),
                device_os_build: None,
                device_os_security_patch: None,
                device_cpu_arch: None,
                device_os_family: None,
                device_chip_model: "".into(),
                device_ram_bytes: 0,
                device_gpu_model: None,
                device_gpu_vram_bytes: None,
                device_npu_model: None,
                device_npu_vram_bytes: None,
            }
        );
        Ok(())
    }

    #[test]
    fn device_info_serializes_flat() -> anyhow::Result<()> {
        let info = DeviceInfo {
            device_name: "Test Device".into(),
            device_form_factor: DeviceFormFactor::Desktop,
            device_os_name: "macOS".into(),
            device_os_version: "15.0".into(),
            device_os_build: Some("24F74".into()),
            device_os_security_patch: None,
            device_cpu_arch: Some("aarch64".into()),
            device_os_family: Some("Darwin".into()),
            device_chip_model: "Apple M3".into(),
            device_ram_bytes: 36_000_000_000,
            device_gpu_model: None,
            device_gpu_vram_bytes: None,
            device_npu_model: None,
            device_npu_vram_bytes: None,
        };
        let value = serde_json::to_value(&info)?;
        assert_eq!(value["device_name"], "Test Device");
        assert_eq!(value["device_form_factor"], "desktop");
        assert_eq!(value["device_ram_bytes"], 36_000_000_000u64);
        assert!(value.get("device_gpu_model").is_none());
        assert_eq!(value["device_os_build"], "24F74");
        assert!(value.get("device_os_security_patch").is_none());
        assert_eq!(value["device_cpu_arch"], "aarch64");
        assert_eq!(value["device_os_family"], "Darwin");
        Ok(())
    }

    #[rstest::rstest]
    #[case("os:ios", true)]
    #[case("os_version:26.1", true)]
    #[case("runtime:llama_cpp:b9999", true)]
    #[case("ram_bytes:17179869184", true)]
    #[case("job_retry", true)] // free-form
    #[case("device:iphone17pro", true)]
    #[case("device:éclair", true)] // non-ASCII lowercase
    #[case("device:Éclair", false)] // non-ASCII uppercase, refused like ASCII
    #[case("", false)] // empty
    #[case("os:iOS", false)] // uppercase
    #[case("OS:ios", false)] // uppercase in namespace
    #[case("os: ios", false)] // internal whitespace
    #[case("os:ios ", false)] // trailing whitespace
    #[case(" os:ios", false)] // leading whitespace
    #[case("os:i\tos", false)] // tab
    fn capability_flag_canonical_form(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(CapabilityFlag::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(42)]
    #[case(99)]
    #[case(100)]
    fn battery_level_accepts_in_range(#[case] v: i32) -> anyhow::Result<()> {
        let parsed: BatteryLevel = serde_json::from_value(json!(v))?;
        assert_eq!(parsed.into_inner(), v);
        Ok(())
    }

    #[rstest]
    #[case(-1)]
    #[case(101)]
    #[case(255)]
    #[case(i32::MIN)]
    #[case(i32::MAX)]
    fn battery_level_rejects_out_of_range(#[case] v: i32) -> anyhow::Result<()> {
        let err = serde_json::from_value::<BatteryLevel>(json!(v))
            .err()
            .context("out-of-range battery level must be rejected")?;
        assert!(err.to_string().contains("between 0 and 100"));
        Ok(())
    }

    #[test]
    fn battery_level_serializes_as_plain_integer() -> anyhow::Result<()> {
        let parsed: BatteryLevel = serde_json::from_value(json!(73))?;
        assert_eq!(serde_json::to_value(parsed)?, json!(73));
        Ok(())
    }
}
