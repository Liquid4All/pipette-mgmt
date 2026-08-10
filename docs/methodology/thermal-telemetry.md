# Thermal telemetry

Optional, **per-platform** thermal telemetry attached to every submission. It is
a **reported environmental condition**, not a controlled benchmark measurement:
the harness does not drive the device to a temperature — it records, best-effort,
whatever each OS exposes, so consumers can **caveat** a result (was the device
throttling? how hot did it get?) rather than treat it as a benchmark axis.

Recorded **verbatim to each vendor's API** — there is **no cross-platform enum
mapping** (Apple's 4-band state and Android's 7-band status are different
vocabularies; collapsing them loses detail). Where a platform exposes **multiple
per-sensor values, the full set is stored as an array** so downstream can reduce
however it wants. Every field is nullable/empty; a row populates only the
families its platform exposes (`device_os_name` disambiguates), and old clients
read back `null`.

## Capture contract

Each family is captured **per measured repetition** as a `before` / `after`
pair of series, bracketing each rep's *timed region*:

- **`_before`** — the reading **must be taken after the thermal readiness gate
  has passed**: the gate holds each rep until the device has settled into the
  stable thermal state required by the
  [Common rules](../benchmarks.md#common-rules), and the sample is captured the
  moment it clears — immediately before the rep's timed work begins (model load
  and warmup are earlier, outside this point). It records the device's **entry /
  gate condition** per rep, never a reading from before the gate cleared.
- **`_after`** — sampled immediately after each repetition's timed work
  completes. The per-rep `after − before` delta is that rep's net thermal rise;
  the series across reps shows the thermal trajectory over the run.

The scalar families (Apple state, Android status, Android headroom) carry one
value per repetition. The sensor/zone families flatten every
(iteration, sensor) pair into a single list, each element tagged with its
zero-based `iteration`.

**No separate `worst` is stored** — the worst thermal condition over a run
(most-severe state/status, highest headroom, hottest sensor/zone) is derivable
from these series downstream, so storing it would be redundant.

## What each platform exposes (verified against vendor docs)

| Platform | Field(s) populated | Source |
|---|---|---|
| **Apple** (iOS / iPadOS / **macOS**) | `device_apple_thermal_state_*` (all Apple); `device_apple_soc_temp_c_*` (iOS, `PIPETTE_PRIVATE_THERMAL` build only) | [`ProcessInfo.thermalState`](https://developer.apple.com/documentation/foundation/processinfo/thermalstate-swift.property) — one device-level enum; Foundation API on macOS (10.10.3+) and iOS (11+), so Macs report through it too. It is the **only public** thermal signal — no supported Apple API returns a temperature. On Apple Silicon an actual °C exists only behind root (`powermetrics`) or the private `IOHIDEventSystem` die sensors (`PMU tdie*`); today the harness reads it only on the entitled **`PIPETTE_PRIVATE_THERMAL`** iOS build, which populates `device_apple_soc_temp_c_*`. |
| **Android** (app SDK) | `device_android_thermal_status_*`, `device_android_thermal_headroom_*` | [`PowerManager.getCurrentThermalStatus()`](https://developer.android.com/reference/android/os/PowerManager) (API 29), `getThermalHeadroom(int)` (API 30). **No temperature in the app SDK.** |
| **Android** (thermal HAL, privileged) | `device_android_thermal_sensors_*` | `android.hardware.thermal` [`Temperature`](https://android.googlesource.com/platform/hardware/interfaces/+/refs/heads/main/thermal/aidl/android/hardware/thermal/Temperature.aidl) — typed per-sensor °C + per-sensor throttling status. Not reachable by an ordinary app (see Caveats). |
| **Linux / embedded** (Jetson, RPi, NUC) | `device_linux_thermal_zones_*` | [`/sys/class/thermal/thermal_zone*`](https://www.kernel.org/doc/Documentation/thermal/sysfs-api.txt) — per-zone `type` + `temp` (milli-°C → °C). No OS state/headroom concept. |

`device_os_name` selects the family set: `iOS` / `iPadOS` / `macOS` → Apple state (iOS on the `PIPETTE_PRIVATE_THERMAL` build also populates `device_apple_soc_temp_c_*`);
`Android` → Android status/headroom (app SDK) and/or sensors (HAL); `Linux` → sysfs zones.

## Enums

Stored lowercase; each is the vendor's own set — **do not conflate them across
vendors** (Android `critical` ≠ Apple `critical` in severity):

- **`device_apple_thermal_state_*`** — Apple `ProcessInfo.ThermalState`:
  `nominal`, `fair`, `serious`, `critical` (coolest → hottest).
- **`device_android_thermal_status_*`** — `AndroidThermalStatus`, from
  `PowerManager.getCurrentThermalStatus()` (`THERMAL_STATUS_*`).
- each sensor's **`throttling_status`** — `AndroidThrottlingSeverity`, from the
  thermal-HAL `ThrottlingSeverity`.

  Both Android enums carry the same seven levels — `none`, `light`, `moderate`,
  `severe`, `critical`, `emergency`, `shutdown` — but are **distinct upstream
  types** (device-level `PowerManager` status vs per-sensor HAL severity), kept
  separate to mirror upstream; there is no mapping between them.

## Field reference

All nullable; each family has a `_before` and an `_after` series. The scalar
families are a `List<scalar>` with one element per repetition; the sensor/zone
families are a `List<Struct>` flattening every (iteration, sensor) pair. For
any list, a **null** list means the family was not captured; an **empty** list
means it was captured but the source reported no values — the two are stored
distinctly and round-trip.

| Field family | Type | Notes |
|---|---|---|
| `device_apple_thermal_state_*` | `List<string (enum)>` | Apple state (above), one per repetition. |
| `device_apple_soc_temp_c_*` | `List<f32>` | Raw iOS SoC die temperature (fractional °C), one per repetition. iOS-only, gated on the `PIPETTE_PRIVATE_THERMAL` client build; stored raw (no rounding/bucketing). The whole array is null when the sensor is unreadable or the flag is off (per-element nulls are not representable). |
| `device_android_thermal_status_*` | `List<string (enum)>` | `AndroidThermalStatus` (above), one per repetition. |
| `device_android_thermal_headroom_*` | `List<f32>` | `getThermalHeadroom(forecastSeconds)` (0–60 s), one per repetition — **fraction of the thermal envelope in use**: `0.0` = coolest, `1.0` = the `SEVERE` threshold, **may exceed 1.0**; higher = worse. |
| `device_android_thermal_sensors_*` | `List<Struct>` | Android thermal-HAL per-sensor readings, flattened across reps (privileged). Element below. |
| `device_linux_thermal_zones_*` | `List<Struct>` | Linux sysfs per-zone readings, flattened across reps. Element below. |

**`device_android_thermal_sensors_*` element:**

| Field | Type | Notes |
|---|---|---|
| `iteration` | int32 | Zero-based index of the measured repetition this reading was sampled at. |
| `type` | string | Lowercased Android `TemperatureType`: `cpu`, `gpu`, `battery`, `skin`, `usb_port`, `power_amplifier`, `npu`, `tpu`, `display`, `modem`, `soc`, `wifi`, `camera`, `flashlight`, `speaker`, `ambient`, `pogo` (`unknown` for unmapped). The `bcl_*` virtual sensors are **excluded** — their HAL value is mV/mA/%, not °C. |
| `name` | string | Vendor sensor name (HAL `Temperature.name`), e.g. `cpu-big`. |
| `celsius` | int32 | Whole degrees °C — the client rounds the HAL `Temperature.value` `float`; stored as reported (no ingest validation). |
| `throttling_status` | string | Per-sensor `AndroidThrottlingSeverity` (enum above). |

**`device_linux_thermal_zones_*` element:**

| Field | Type | Notes |
|---|---|---|
| `iteration` | int32 | Zero-based index of the measured repetition this reading was sampled at. |
| `type` | string | sysfs zone `type`, e.g. `x86_pkg_temp`, `acpitz`, `cpu-thermal`. |
| `celsius` | int32 | Whole degrees °C (client converts + rounds sysfs milli-°C). |

**Temperature semantics.** All temperatures are whole degrees °C (`int32`).
There is **no single "max temperature" scalar** — "hottest zone" is derived
downstream from the arrays. Values are **stored as reported** (no ingest range
validation); clients send a plausible reading or omit the element, and obviously
bad readings are filtered downstream.

## Caveats (verified against vendor docs)

- **Availability / gating.** Android status is API 29+, headroom API 30+ — both
  read `null` on older devices. Headroom is **rate-limited** (~1 Hz; calling
  faster may return `NaN`) and needs a few seconds of warm-up before it
  forecasts; an unsupported device returns `NaN` → store `null`. Apple
  `thermalState` always returns a value. Per-sensor temperatures require the
  **privileged** Android thermal HAL — no ordinary-app path (the SDK's
  `HardwarePropertiesManager` is Device-Owner/VR-only and covers just
  CPU/GPU/battery/skin) — or Linux sysfs.
- **`nominal` / `none` is ambiguous** — a device whose OEM never wired up thermal
  reporting reads the coolest bucket throughout, indistinguishable from a
  genuinely cool run. Treat a flat coolest reading under a hot workload with
  suspicion.
- **Coarse by design.** Temperatures are whole °C (sub-degree drift is
  intentionally not retained). The state/status enums are advisory severity
  bands set by each OS's own policy; the device-level status/state is an opaque
  aggregate, not a documented cross-sensor max. The run's worst condition is
  derived downstream from the per-iteration series, not stored.
- **System-wide, not per-process.** Apple/Android device-level signals reflect
  the whole device (ambient, other apps), not just the benchmark — attribute
  causation cautiously.

## Sources

- Apple `ProcessInfo.thermalState` — [enum cases](https://developer.apple.com/documentation/foundation/processinfo/thermalstate-swift.enum) · [property](https://developer.apple.com/documentation/foundation/processinfo/thermalstate-swift.property) · [Respond to Thermal State Changes (Mac)](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/RespondToThermalStateChanges.html)
- Android `PowerManager` (`getCurrentThermalStatus` API 29, `getThermalHeadroom` API 30) — [reference](https://developer.android.com/reference/android/os/PowerManager) · [Thermal mitigation](https://source.android.com/docs/core/power/thermal-mitigation)
- Android thermal HAL — [`Temperature.aidl`](https://android.googlesource.com/platform/hardware/interfaces/+/refs/heads/main/thermal/aidl/android/hardware/thermal/Temperature.aidl) · [`TemperatureType.aidl`](https://android.googlesource.com/platform/hardware/interfaces/+/refs/heads/main/thermal/aidl/android/hardware/thermal/TemperatureType.aidl) · [`ThrottlingSeverity.aidl`](https://android.googlesource.com/platform/hardware/interfaces/+/refs/heads/main/thermal/aidl/android/hardware/thermal/ThrottlingSeverity.aidl)
- Linux thermal sysfs — [`sysfs-api`](https://www.kernel.org/doc/Documentation/thermal/sysfs-api.txt)

## Per-platform examples

Concrete submission payloads (thermal fields only), one per platform. A device
fills only its own families; the rest are omitted (`null`).

These examples model a 3-repetition run. Scalar families carry one value per
rep; sensor/zone lists flatten every (iteration, sensor) pair.

**Apple** (iOS / iPadOS / macOS) — `thermal_state` on every Apple device; the `PIPETTE_PRIVATE_THERMAL` iOS build additionally emits the raw SoC die temperature:

```jsonc
"device_apple_thermal_state_before": ["nominal", "nominal", "fair"],
"device_apple_thermal_state_after":  ["nominal", "fair",    "serious"],
// PIPETTE_PRIVATE_THERMAL iOS build only — raw SoC die temperature (°C):
"device_apple_soc_temp_c_before": [41.5, 43.0, 46.25],
"device_apple_soc_temp_c_after":  [43.0, 46.25, 49.5]
```

**Android** — status + headroom (app SDK) and the thermal-HAL per-sensor array
(privileged). Headroom **rises** over the run (higher = closer to throttling):

```jsonc
"device_android_thermal_status_before": ["none", "none",  "light"],
"device_android_thermal_status_after":  ["none", "light", "moderate"],
"device_android_thermal_headroom_before": [0.31, 0.44, 0.58],
"device_android_thermal_headroom_after":  [0.42, 0.55, 0.66],
"device_android_thermal_sensors_before": [
  { "iteration": 0, "type": "cpu",     "name": "cpu-0-0-usr",  "celsius": 38, "throttling_status": "none" },
  { "iteration": 0, "type": "skin",    "name": "VIRTUAL-SKIN", "celsius": 33, "throttling_status": "none" },
  { "iteration": 1, "type": "cpu",     "name": "cpu-0-0-usr",  "celsius": 49, "throttling_status": "none" },
  { "iteration": 1, "type": "skin",    "name": "VIRTUAL-SKIN", "celsius": 38, "throttling_status": "none" },
  { "iteration": 2, "type": "cpu",     "name": "cpu-0-0-usr",  "celsius": 61, "throttling_status": "light" },
  { "iteration": 2, "type": "skin",    "name": "VIRTUAL-SKIN", "celsius": 42, "throttling_status": "none" }
]
```

**Linux / embedded** (Jetson, RPi, NUC) — sysfs zones only:

```jsonc
"device_linux_thermal_zones_before": [
  { "iteration": 0, "type": "x86_pkg_temp", "celsius": 44 },
  { "iteration": 0, "type": "acpitz",       "celsius": 41 },
  { "iteration": 1, "type": "x86_pkg_temp", "celsius": 55 },
  { "iteration": 1, "type": "acpitz",       "celsius": 50 }
],
"device_linux_thermal_zones_after": [
  { "iteration": 0, "type": "x86_pkg_temp", "celsius": 52 },
  { "iteration": 0, "type": "acpitz",       "celsius": 48 },
  { "iteration": 1, "type": "x86_pkg_temp", "celsius": 63 },
  { "iteration": 1, "type": "acpitz",       "celsius": 58 }
]
```

(ARM boards report zone `type`s like `cpu-thermal` / `gpu-thermal` instead of the
x86 `x86_pkg_temp` / `acpitz`.)

## Storage

The columns live in the warehouse Parquet schema alongside the other common
columns — see [`benchmarks.md` § Parquet storage schema](../benchmarks.md#31-parquet-storage-schema).
Client capture (which API per platform, per-iteration before/after timing,
privilege needs, `NaN`/rate-limit handling, `bcl_*` exclusion) is owned by
`pipette-clients`.
