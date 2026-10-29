import Foundation
import UIKit

/// What this device *is*, and what state it is in — iOS's `pipette-device`.
///
/// The crate splits detection from shape: `pipette_device::probe` reads the host and
/// returns `pipette_plan_types::device::DeviceInfo`. The same split holds here, which
/// is why the payload struct lives in `PlanTypes/Device.swift` and only the reading
/// lives in this file. Per-field detectors carry the crate's `detect*` verb.
///
/// Every value comes from a `sysctl`, `ProcessInfo`, or `UIDevice`, none of which can
/// fail, so `detectDeviceInfo(deviceName:formFactor:)` returns a value where the crate's
/// returns a `Result`. The operator's `device.json` labels (`DeviceLabels`) override the
/// detected name and form factor there, as they do in the crate.
///
/// MainActor-isolated by the project default, which `UIDevice` requires.
enum DeviceProbe {

    /// This device's full identity, the crate's `detect_device_info`. Single-sourced so
    /// the profile patch and the submission payload cannot disagree about the device
    /// they describe.
    ///
    /// `deviceName` and `formFactor` are the operator's labels
    /// (`IdentityStore.getDeviceLabels()`), and win over the detected values when set. A
    /// name is trimmed, and a blank one counts as unset. The crate's filter would then fail
    /// detection for want of a name; this falls back to the detected one instead, since
    /// nothing here can fail.
    static func detectDeviceInfo(deviceName: String?, formFactor: DeviceFormFactor?) -> DeviceInfo {
        DeviceInfo(
            deviceName: nonBlank(deviceName) ?? detectDeviceName(),
            deviceFormFactor: formFactor ?? detectFormFactor(),
            deviceOsName: detectOsName(),
            deviceOsVersion: detectOsVersion(),
            deviceOsBuild: detectOsBuild(),
            // No iOS API exposes a security-patch level; see `DeviceInfo`.
            deviceOsSecurityPatch: nil,
            deviceChipModel: detectChipModel(),
            deviceRamBytes: detectRamBytes(),
            deviceGpuModel: nil,
            deviceGpuVramBytes: nil,
            deviceNpuModel: nil,
            deviceNpuVramBytes: nil)
    }

    /// `value` trimmed, or nil when that leaves nothing: the crate's `TrimmedString` and
    /// its empty filter.
    private static func nonBlank(_ value: String?) -> String? {
        guard let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines),
              !trimmed.isEmpty else { return nil }
        return trimmed
    }

    /// The volatile run-environment state, the crate's `detect_power_state`. Separate
    /// from `detectDeviceInfo` for the reason the crate separates them: this changes
    /// between two runs on one device, so it describes a measurement rather than the
    /// host, and rides on submissions only — never on the profile.
    static func detectPowerState() -> PowerState {
        PowerState(
            batteryLevel: detectBatteryLevel(),
            powerState: detectDevicePowerState(),
            powerSaveMode: detectPowerSaveMode())
    }

    /// The die reading as recorded: whole °C, the crate's `recorded_die_temp_c`. The wire
    /// column is 32-bit, so a fractional value would reach a reader as 46.79999923706055
    /// rather than the number recorded; `ThermalSeries` rounds through this so the iOS
    /// column rounds as the macOS one does (half away from zero, as Rust's `round`).
    nonisolated static func recordedDieTempC(_ raw: Double) -> Float {
        Float(raw.rounded(.toNearestOrAwayFromZero))
    }

    // MARK: - Identity

    private static let machineIdentifier: String = {
        var size = 0
        sysctlbyname("hw.machine", nil, &size, nil, 0)
        var machine = [CChar](repeating: 0, count: size)
        sysctlbyname("hw.machine", &machine, &size, nil, 0)
        return String(cString: machine)
    }()

    /// The human-readable device model name (e.g. "iPhone 16 Pro Max"), the crate's
    /// `device_name`. Falls back to the raw machine identifier if unknown.
    static func detectDeviceName() -> String {
        modelNameMapping[machineIdentifier] ?? machineIdentifier
    }

    /// The SoC model (e.g. "Apple A18 Pro").
    /// Falls back to the raw machine identifier if unknown.
    static func detectChipModel() -> String {
        chipModelMapping[machineIdentifier] ?? machineIdentifier
    }

    /// `.tablet` for iPad, `.phone` otherwise — the only two arms this platform reaches.
    static func detectFormFactor() -> DeviceFormFactor {
        machineIdentifier.hasPrefix("iPad") ? .tablet : .phone
    }

    /// The OS name: "iPadOS" for iPad, "iOS" otherwise.
    static func detectOsName() -> String {
        machineIdentifier.hasPrefix("iPad") ? "iPadOS" : "iOS"
    }

    /// The OS version string (e.g. "18.4" or "26.4").
    static func detectOsVersion() -> String {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        return v.patchVersion == 0
            ? "\(v.majorVersion).\(v.minorVersion)"
            : "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)"
    }

    /// The OS build identifier (e.g. "22F76") from `kern.osversion` — finer-grained
    /// than `detectOsVersion()`. Nil if the sysctl is unreadable or empty.
    static func detectOsBuild() -> String? {
        var size = 0
        guard sysctlbyname("kern.osversion", nil, &size, nil, 0) == 0, size > 0 else { return nil }
        var build = [CChar](repeating: 0, count: size)
        guard sysctlbyname("kern.osversion", &build, &size, nil, 0) == 0 else { return nil }
        let value = String(cString: build)
        return value.isEmpty ? nil : value
    }

    /// Total system RAM in bytes.
    static func detectRamBytes() -> UInt64 {
        ProcessInfo.processInfo.physicalMemory
    }

    // MARK: - Run-environment power state

    // Captured with each result so on-battery runs (where the PMIC can cap CPU clocks
    // to avoid voltage sag — distinct from thermal throttling) and Low Power Mode runs
    // can be filtered/flagged after the fact.

    /// Battery charge percent (0–100), or nil when unavailable (e.g. the
    /// simulator reports -1). Enables battery monitoring as a side effect.
    static func detectBatteryLevel() -> Int32? {
        UIDevice.current.isBatteryMonitoringEnabled = true
        let level = UIDevice.current.batteryLevel  // 0.0–1.0, or -1.0 if unknown
        guard level >= 0 else { return nil }
        return Int32((level * 100).rounded())
    }

    /// Charging state, or nil when unknown. Mirrors Android's `DeviceInfo.powerState`.
    static func detectDevicePowerState() -> DevicePowerState? {
        UIDevice.current.isBatteryMonitoringEnabled = true
        switch UIDevice.current.batteryState {
        case .charging: return .charging
        case .unplugged: return .notCharging
        case .full: return .pluggedInNotCharging
        case .unknown: return nil
        @unknown default: return nil
        }
    }

    /// Whether Low Power Mode is enabled (can lower CPU/GPU clocks).
    static func detectPowerSaveMode() -> Bool {
        ProcessInfo.processInfo.isLowPowerModeEnabled
    }

    // MARK: - Private mappings

    private static let modelNameMapping: [String: String] = [
        // iPhone 18 series
        "iPhone19,2": "iPhone 18 Pro",
        "iPhone19,3": "iPhone 18 Pro Max",
        "iPhone19,4": "iPhone Duo",
        "iPhone19,7": "iPhone 18 Pro Max",
        // iPhone 17 series
        "iPhone18,1": "iPhone 17 Pro",
        "iPhone18,2": "iPhone 17 Pro Max",
        "iPhone18,3": "iPhone 17",
        "iPhone18,4": "iPhone Air",
        "iPhone18,5": "iPhone 17e",
        // iPhone 16 series
        "iPhone17,1": "iPhone 16 Pro",
        "iPhone17,2": "iPhone 16 Pro Max",
        "iPhone17,3": "iPhone 16",
        "iPhone17,4": "iPhone 16 Plus",
        "iPhone17,5": "iPhone 16e",
        // iPhone 15 series
        "iPhone16,1": "iPhone 15 Pro",
        "iPhone16,2": "iPhone 15 Pro Max",
        "iPhone15,4": "iPhone 15",
        "iPhone15,5": "iPhone 15 Plus",
        // iPhone 14 series
        "iPhone15,2": "iPhone 14 Pro",
        "iPhone15,3": "iPhone 14 Pro Max",
        "iPhone14,7": "iPhone 14",
        "iPhone14,8": "iPhone 14 Plus",
        // iPhone 13 series
        "iPhone14,2": "iPhone 13 Pro",
        "iPhone14,3": "iPhone 13 Pro Max",
        "iPhone14,5": "iPhone 13",
        "iPhone14,4": "iPhone 13 mini",
        // iPhone 12 series
        "iPhone13,3": "iPhone 12 Pro",
        "iPhone13,4": "iPhone 12 Pro Max",
        "iPhone13,2": "iPhone 12",
        "iPhone13,1": "iPhone 12 mini",
        // iPhone 11 series
        "iPhone12,3": "iPhone 11 Pro",
        "iPhone12,5": "iPhone 11 Pro Max",
        "iPhone12,1": "iPhone 11",
        // iPhone X series
        "iPhone11,2": "iPhone XS",
        "iPhone11,4": "iPhone XS Max",
        "iPhone11,6": "iPhone XS Max",
        "iPhone11,8": "iPhone XR",
        // iPhone SE
        "iPhone14,6": "iPhone SE (3rd gen)",
        "iPhone12,8": "iPhone SE (2nd gen)",
        // iPad, Apple A12 Bionic
        "iPad11,1": "iPad mini (5th gen)",
        "iPad11,2": "iPad mini (5th gen)",
        "iPad11,3": "iPad Air (3rd gen)",
        "iPad11,4": "iPad Air (3rd gen)",
        "iPad11,6": "iPad (8th gen)",
        "iPad11,7": "iPad (8th gen)",
        // iPad, Apple A12X Bionic
        "iPad8,1": "iPad Pro 11-inch (1st gen)",
        "iPad8,2": "iPad Pro 11-inch (1st gen)",
        "iPad8,3": "iPad Pro 11-inch (1st gen)",
        "iPad8,4": "iPad Pro 11-inch (1st gen)",
        "iPad8,5": "iPad Pro 12.9-inch (3rd gen)",
        "iPad8,6": "iPad Pro 12.9-inch (3rd gen)",
        "iPad8,7": "iPad Pro 12.9-inch (3rd gen)",
        "iPad8,8": "iPad Pro 12.9-inch (3rd gen)",
        // iPad, Apple A12Z Bionic
        "iPad8,9": "iPad Pro 11-inch (2nd gen)",
        "iPad8,10": "iPad Pro 11-inch (2nd gen)",
        "iPad8,11": "iPad Pro 12.9-inch (4th gen)",
        "iPad8,12": "iPad Pro 12.9-inch (4th gen)",
        // iPad, Apple A13 Bionic
        "iPad12,1": "iPad (9th gen)",
        "iPad12,2": "iPad (9th gen)",
        // iPad, Apple A14 Bionic
        "iPad13,1": "iPad Air (4th gen)",
        "iPad13,2": "iPad Air (4th gen)",
        "iPad13,18": "iPad (10th gen)",
        "iPad13,19": "iPad (10th gen)",
        // iPad, Apple A15 Bionic
        "iPad14,1": "iPad mini (6th gen)",
        "iPad14,2": "iPad mini (6th gen)",
        // iPad, Apple A16
        "iPad15,7": "iPad (A16)",
        "iPad15,8": "iPad (A16)",
        // iPad, Apple A17 Pro
        "iPad16,1": "iPad mini (A17 Pro)",
        "iPad16,2": "iPad mini (A17 Pro)",
        // iPad, Apple M1
        "iPad13,4": "iPad Pro 11-inch (3rd gen)",
        "iPad13,5": "iPad Pro 11-inch (3rd gen)",
        "iPad13,6": "iPad Pro 11-inch (3rd gen)",
        "iPad13,7": "iPad Pro 11-inch (3rd gen)",
        "iPad13,8": "iPad Pro 12.9-inch (5th gen)",
        "iPad13,9": "iPad Pro 12.9-inch (5th gen)",
        "iPad13,10": "iPad Pro 12.9-inch (5th gen)",
        "iPad13,11": "iPad Pro 12.9-inch (5th gen)",
        "iPad13,16": "iPad Air (5th gen)",
        "iPad13,17": "iPad Air (5th gen)",
        // iPad, Apple M2
        "iPad14,3": "iPad Pro 11-inch (4th gen)",
        "iPad14,4": "iPad Pro 11-inch (4th gen)",
        "iPad14,5": "iPad Pro 12.9-inch (6th gen)",
        "iPad14,6": "iPad Pro 12.9-inch (6th gen)",
        "iPad14,8": "iPad Air 11-inch (M2)",
        "iPad14,9": "iPad Air 11-inch (M2)",
        "iPad14,10": "iPad Air 13-inch (M2)",
        "iPad14,11": "iPad Air 13-inch (M2)",
        // iPad, Apple M3
        "iPad15,3": "iPad Air 11-inch (M3)",
        "iPad15,4": "iPad Air 11-inch (M3)",
        "iPad15,5": "iPad Air 13-inch (M3)",
        "iPad15,6": "iPad Air 13-inch (M3)",
        // iPad, Apple M4
        "iPad16,3": "iPad Pro 11-inch (M4)",
        "iPad16,4": "iPad Pro 11-inch (M4)",
        "iPad16,5": "iPad Pro 13-inch (M4)",
        "iPad16,6": "iPad Pro 13-inch (M4)",
        "iPad16,8": "iPad Air 11-inch (M4)",
        "iPad16,9": "iPad Air 11-inch (M4)",
        "iPad16,10": "iPad Air 13-inch (M4)",
        "iPad16,11": "iPad Air 13-inch (M4)",
        // iPad, Apple M5
        "iPad17,1": "iPad Pro 11-inch (M5)",
        "iPad17,2": "iPad Pro 11-inch (M5)",
        "iPad17,3": "iPad Pro 13-inch (M5)",
        "iPad17,4": "iPad Pro 13-inch (M5)",
    ]

    private static let chipModelMapping: [String: String] = [
        // iPhone 18 series
        "iPhone19,2": "Apple A20 Pro",
        "iPhone19,3": "Apple A20 Pro",
        "iPhone19,4": "Apple A20 Pro",
        "iPhone19,7": "Apple A20 Pro",
        // iPhone 17 series
        "iPhone18,1": "Apple A19 Pro",
        "iPhone18,2": "Apple A19 Pro",
        "iPhone18,3": "Apple A19",
        "iPhone18,4": "Apple A19 Pro",
        "iPhone18,5": "Apple A19",
        // iPhone 16 series
        "iPhone17,1": "Apple A18 Pro",
        "iPhone17,2": "Apple A18 Pro",
        "iPhone17,3": "Apple A18",
        "iPhone17,4": "Apple A18",
        "iPhone17,5": "Apple A18",
        // iPhone 15 series
        "iPhone16,1": "Apple A17 Pro",
        "iPhone16,2": "Apple A17 Pro",
        "iPhone15,4": "Apple A16 Bionic",
        "iPhone15,5": "Apple A16 Bionic",
        // iPhone 14 series
        "iPhone15,2": "Apple A16 Bionic",
        "iPhone15,3": "Apple A16 Bionic",
        "iPhone14,7": "Apple A15 Bionic",
        "iPhone14,8": "Apple A15 Bionic",
        // iPhone 13 series
        "iPhone14,2": "Apple A15 Bionic",
        "iPhone14,3": "Apple A15 Bionic",
        "iPhone14,5": "Apple A15 Bionic",
        "iPhone14,4": "Apple A15 Bionic",
        // iPhone 12 series
        "iPhone13,3": "Apple A14 Bionic",
        "iPhone13,4": "Apple A14 Bionic",
        "iPhone13,2": "Apple A14 Bionic",
        "iPhone13,1": "Apple A14 Bionic",
        // iPhone 11 series
        "iPhone12,3": "Apple A13 Bionic",
        "iPhone12,5": "Apple A13 Bionic",
        "iPhone12,1": "Apple A13 Bionic",
        // iPhone X series
        "iPhone11,2": "Apple A12 Bionic",
        "iPhone11,4": "Apple A12 Bionic",
        "iPhone11,6": "Apple A12 Bionic",
        "iPhone11,8": "Apple A12 Bionic",
        // iPhone SE
        "iPhone14,6": "Apple A15 Bionic",
        "iPhone12,8": "Apple A13 Bionic",
        // iPad, Apple A12 Bionic
        "iPad11,1": "Apple A12 Bionic",
        "iPad11,2": "Apple A12 Bionic",
        "iPad11,3": "Apple A12 Bionic",
        "iPad11,4": "Apple A12 Bionic",
        "iPad11,6": "Apple A12 Bionic",
        "iPad11,7": "Apple A12 Bionic",
        // iPad, Apple A12X Bionic
        "iPad8,1": "Apple A12X Bionic",
        "iPad8,2": "Apple A12X Bionic",
        "iPad8,3": "Apple A12X Bionic",
        "iPad8,4": "Apple A12X Bionic",
        "iPad8,5": "Apple A12X Bionic",
        "iPad8,6": "Apple A12X Bionic",
        "iPad8,7": "Apple A12X Bionic",
        "iPad8,8": "Apple A12X Bionic",
        // iPad, Apple A12Z Bionic
        "iPad8,9": "Apple A12Z Bionic",
        "iPad8,10": "Apple A12Z Bionic",
        "iPad8,11": "Apple A12Z Bionic",
        "iPad8,12": "Apple A12Z Bionic",
        // iPad, Apple A13 Bionic
        "iPad12,1": "Apple A13 Bionic",
        "iPad12,2": "Apple A13 Bionic",
        // iPad, Apple A14 Bionic
        "iPad13,1": "Apple A14 Bionic",
        "iPad13,2": "Apple A14 Bionic",
        "iPad13,18": "Apple A14 Bionic",
        "iPad13,19": "Apple A14 Bionic",
        // iPad, Apple A15 Bionic
        "iPad14,1": "Apple A15 Bionic",
        "iPad14,2": "Apple A15 Bionic",
        // iPad, Apple A16
        "iPad15,7": "Apple A16",
        "iPad15,8": "Apple A16",
        // iPad, Apple A17 Pro
        "iPad16,1": "Apple A17 Pro",
        "iPad16,2": "Apple A17 Pro",
        // iPad, Apple M1
        "iPad13,4": "Apple M1",
        "iPad13,5": "Apple M1",
        "iPad13,6": "Apple M1",
        "iPad13,7": "Apple M1",
        "iPad13,8": "Apple M1",
        "iPad13,9": "Apple M1",
        "iPad13,10": "Apple M1",
        "iPad13,11": "Apple M1",
        "iPad13,16": "Apple M1",
        "iPad13,17": "Apple M1",
        // iPad, Apple M2
        "iPad14,3": "Apple M2",
        "iPad14,4": "Apple M2",
        "iPad14,5": "Apple M2",
        "iPad14,6": "Apple M2",
        "iPad14,8": "Apple M2",
        "iPad14,9": "Apple M2",
        "iPad14,10": "Apple M2",
        "iPad14,11": "Apple M2",
        // iPad, Apple M3
        "iPad15,3": "Apple M3",
        "iPad15,4": "Apple M3",
        "iPad15,5": "Apple M3",
        "iPad15,6": "Apple M3",
        // iPad, Apple M4
        "iPad16,3": "Apple M4",
        "iPad16,4": "Apple M4",
        "iPad16,5": "Apple M4",
        "iPad16,6": "Apple M4",
        "iPad16,8": "Apple M4",
        "iPad16,9": "Apple M4",
        "iPad16,10": "Apple M4",
        "iPad16,11": "Apple M4",
        // iPad, Apple M5
        "iPad17,1": "Apple M5",
        "iPad17,2": "Apple M5",
        "iPad17,3": "Apple M5",
        "iPad17,4": "Apple M5",
    ]
}
