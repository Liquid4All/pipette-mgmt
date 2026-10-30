# Design: device telemetry as each OS reports it

**Status: mgmt accepts and stores the three fields. The iOS app sends all
three; the CLI sends `readiness_gate_waits` everywhere and the readings on
Windows. The CLI elsewhere, and the Android app, still send the twelve
per-platform thermal fields.** How mgmt stores the three is
[storage.md](../storage.md#device_hardware_readings--device_os_throttling_readings--readiness_gate_waits);
what each platform reads and posts is the clients repo's
[device-conditions.md](https://github.com/Liquid4All/pipette-clients/blob/main/docs/methodology/device-conditions.md).

## Why

- **Every new platform was a schema change in two repositories.** Each OS got
  its own `_before` / `_after` pair in plan-types, on the wire and in the
  warehouse. Windows would have been the seventh pair.
- **The translation lost what the OS said.** Clients rounded every temperature
  to a whole degree, kept only the hottest Apple die sensor, dropped Android's
  `BCL_*` sensors and the Linux zone number, and mapped every code to a name. A
  code with no name cost the whole series.
- **An unknown value lost the whole result.** The state fields are enums on the
  server too, so a token a newer client sends is a `400`.
- **The gates read more than anyone recorded.** The macOS thermal pressure
  level, the Pi 5 firmware throttle bits, the Windows zones and GPU temperature,
  the iOS temperature estimate: the gates read each one before every rep, and
  none was stored. Neither was how long each rep waited.
- **Nothing in mgmt reads these fields.** The scorer copies them into the row
  and analysts read them there, so a shape the server imposes buys it nothing.

## The decision

Three fields replace the twelve. mgmt stores each as a JSON array and reads
nothing inside its elements; each client posts what its OS returned, in the
OS's own names, units and codes.

| Field | Holds |
|---|---|
| `device_hardware_readings` | what the device's sensors report, in the records the OS returns them in |
| `device_os_throttling_readings` | what the OS or firmware reports about its own throttling: states, levels, bitmasks, headroom |
| `readiness_gate_waits` | what the client's readiness gate reported before each rep |

`host_readiness` is the gate's configuration; `readiness_gate_waits` is what the
gate did with it.

The server enforces only the array. The shape below is what lets a query find
one platform's readings next to another's:

- **One element per rep and sample point.** `iteration` is the zero-based
  measured rep. `sample_point` is `before_rep`, taken when the gate releases the
  rep, or `after_rep`, taken when its timed work ends.
- **One key per facility read at that moment**, named for the platform family
  and the facility, spelled out: `linux_sysfs_thermal_zones`,
  `android_thermal_hal_temperatures`. A facility that failed or does not exist
  is an absent key.
- **The value is what the facility returned**: nothing rounded, renamed or
  dropped, and no code mapped to a name.
- **`readiness_gate_waits` has one element per rep:** `iteration`,
  `poll_count`, and the gate's own report of its last poll.

## Keys for the platforms without a reader

The clients' device-conditions.md covers the Windows and iOS keys. Until the
other platforms have a reader, their keys are:

| Key | Field | Holds | Replaces |
|---|---|---|---|
| `linux_sysfs_thermal_zones` | hardware | each zone's `type` and `temp` in millidegrees, with the zone (`thermal_zone<N>`) | `device_linux_thermal_zones_*` |
| `raspberry_pi_vcgencmd_get_throttled` | throttling | the line `vcgencmd get_throttled` prints | nothing; the Pi 5 gate reads it |
| `android_thermal_hal_temperatures` | hardware | the HAL's `Temperature` records, codes unmapped, `BCL_*` kept | `device_android_thermal_sensors_*` |
| `android_power_manager_thermal_status` | throttling | the `THERMAL_STATUS_*` code | `device_android_thermal_status_*` |
| `android_power_manager_thermal_headroom` | throttling | `{forecastSeconds, headroom}` | `device_android_thermal_headroom_*` |
| `apple_thermal_pressure_level` | throttling | the macOS `com.apple.system.thermalpressurelevel` state, 0 to 4 | nothing; the macOS gate reads it |

On macOS the iOS keys apply too: `apple_process_info_thermal_state` replaces
`device_apple_thermal_state_*`, and `apple_iohid_temperature_sensors` replaces
`device_apple_soc_temp_c_*`. Every legacy pair has a key, so dropping the twelve
loses nothing.

## Rollout

1. **mgmt accepts and stores the three fields.** Done. It ignores unknown
   fields, so mgmt had to go first.
2. **Clients send them instead of the twelve.** Done for the iOS app, and for
   the CLI on Windows. The CLI elsewhere, and the Android app, still send the
   twelve.
3. **mgmt stops accepting the twelve** once those clients post the keys above
   and the warehouse shows no `client_version` still sending them. Until then
   mgmt keeps accepting them. Their columns stay for the history.

## Open questions

- **Decoding codes.** Rows carry vendor codes, so a query that wants names needs
  the vendor's table. A warehouse view that decodes the common ones would keep
  that out of every query.
- **Load.** Outside Windows, the gates' CPU and GPU load readings exist only
  inside their `observed` lines. If they matter beyond the gate, each becomes a
  hardware reading under its own facility key.
