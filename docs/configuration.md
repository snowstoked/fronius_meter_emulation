# Configuration

Everything is configured with environment variables, so that the container needs no mounted files.
The only required setting is the source meter: `SHELLY_MODBUS` **or** `SHELLY_REST`.

- [Source meter (input)](#source-meter-input)
- [Emulated meter (output)](#emulated-meter-output)
- [Timing](#timing)
- [Home Assistant offset](#home-assistant-offset)
- [Logging and failure behaviour](#logging-and-failure-behaviour)
- [Examples](#examples)

## Source meter (input)

The emulator reads a Shelly and turns its readings into the registers of a Fronius Smart Meter. There are two ways
to read the Shelly. If both variables are set, `SHELLY_REST` is used.

| Variable | Default | Description |
|---|---|---|
| `SHELLY_MODBUS` | | `ip:port` of the Shelly's Modbus TCP server, e.g. `192.168.1.5:502`. Modbus TCP has to be enabled in the Shelly's settings. |
| `SHELLY_REST` | | Address of the Shelly for HTTP RPC ("REST") reads: `192.168.1.5`, `192.168.1.5:80` or a full `http://…` URL. |
| `SHELLY_ID` | lowest id found | Component id to read if a device has several (e.g. channel 1 of a Pro 2PM). `SHELLY_EM_ID` is the old name and still works. Only used with `SHELLY_REST`. |
| `SHELLY_INVERT` | `false` | Flips the direction of power flow: the sign of all power values and power factors, and swaps import and export energy. Works for every source. See [Sign of the power](#sign-of-the-power). |

### Supported Shelly devices

Modbus is the faster source (median read 1 ms against about 55 ms for REST, see
[latency](inverter-communication.md#latency)). REST exists so that devices without Modbus can be used as well, and
it returns all values of one measurement together.

| Device | Modbus | REST | Reported as | Status |
|---|---|---|---|---|
| Shelly Pro 3EM | yes | yes | three phases | Both tested |
| Shelly Pro 1PM | no | yes | one phase (A), phases B and C are 0 | REST tested |
| Shelly Plus 1PM, Pro 2PM, Plus 2PM and other Gen2+ devices with power metering | no | yes | one phase (A) | Expected to work (same `Switch` component), not tested. Use `SHELLY_ID` to pick the channel of a multi-channel device. |
| Gen1 devices (Shelly EM, 3EM) | | no | | Not supported (different API) |
| Devices that only have `em1:N` components (Pro EM-50, EM Gen3) | | no | | Not supported yet. The error message lists the components found. |

How REST detects the device: the emulator asks `Shelly.GetStatus` once and looks at the components.

| Component | Device type | Calls per poll | Energy counters |
|---|---|---|---|
| `em:N` | Energy meter (Pro 3EM) | `EM.GetStatus`: total power, all phases and frequency from one sample | `EMData.GetStatus`, at the slow interval |
| `switch:N` | Relay with power metering (Pro 1PM, …) | `Switch.GetStatus`: power, voltage, current, frequency and energy | Included in every poll |

Details for REST:

- The Shelly must be a **Gen2 or newer** device (it must have the `/rpc` interface), and **authentication has to be
  disabled** (digest authentication is not implemented).
- If a component is not found, or detection fails, the emulator retries on the next poll. The log shows which
  component was detected (`Detected Shelly component switch:0`).
- A relay measures a load, so a positive power means the device consumes. If the direction is the wrong way
  round for what you want to emulate, set `SHELLY_INVERT=true`.
- If the Shelly does not report a power factor, it is derived from power, voltage and current.

For Modbus the registers of the Shelly's `EM` component are read. A Pro 3EM has been seen to serve them at
`1000+` instead of the documented `31000+`. Both are tried on the first read, and the one that works is used.

### Sign of the power

The emulated meter follows the Fronius convention for a meter at the grid connection point: **positive = import**.
That is the same direction as a Shelly 3EM measuring the grid. For a relay such as the Pro 1PM, positive means the
connected device consumes. The inverter shows a consuming load on a *production* meter as negative (observed,
see [sign on a production meter](inverter-communication.md#sign-on-a-production-meter)), so for a battery that is
measured by a 1PM `SHELLY_INVERT` is probably not needed. Check it while the battery discharges.

## Emulated meter (output)

| Variable | Default | Description |
|---|---|---|
| `FRONIUS_MODBUS_BIND` | `0.0.0.0:1502` | `ip:port` the Modbus TCP server listens on. This is the IP and port that you register in the inverter. |
| `FRONIUS_SERIAL` | the Modbus address, e.g. `00000002` | Serial number announced to the inverter, up to 16 characters. If not set, the serial is the Modbus address that the inverter uses in its requests, padded with zeros to 8 digits (`00000001` for address 1). |
| `FRONIUS_MODBUS_ADDRESS` | `240` | Device address in the identity block of the SunSpec data. The name matches the field "Modbus Address" in the inverter. `FRONIUS_DEVICE_ADDRESS` is the old name and still works. Not needed in practice, the inverter does not seem to check it. |

What matters for running several emulators on one inverter, for example a grid meter and an AC battery as
"production meter":

- Each emulator must be reachable on **its own IP address**. The inverter refuses to register two meters with the
  same IP. Port 1502 can stay the same.
- Each meter needs its own **Modbus address** in the inverter (1 and 2, say).
- Each meter needs its own **serial number**. With the default this happens automatically from the Modbus address.
  Set `FRONIUS_SERIAL` only if you want a different serial, or if one emulator is registered under several
  addresses and should always answer with the same serial.
- Requests with unit ID 0 or 255 (broadcast, or the "any unit" ID that TCP clients often use) and reserved
  addresses above 247 get the default serial `00000001`.

The emulator only supports reading (function codes 3 and 4). Writes are refused.

## Timing

| Variable | Default | Description |
|---|---|---|
| `SHELLY_POLL_INTERVAL_MS` | `500` | How often the Shelly is polled for the total power. Minimum 100. A Shelly only measures about once per second, so values below 500 gain little. |
| `SLOW_METER_READ_INTERVAL_S` | `60` | How often the slower values are refreshed (see below). |

What "slow" means depends on the source:

- **Modbus:** phases, frequency and energy counters. They are read in five small requests, one per poll
  cycle, so they never delay the power reading. A complete refresh therefore takes `interval + 5 × poll interval`
  (7.5 s with the values 5 and 500). The first refresh is done right after start-up, and is retried after 10 s
  if it failed.
- **REST, Pro 3EM:** only the energy counters. Phases, power and frequency are fresh on every poll.
- **REST, relays (Pro 1PM, …):** nothing, everything is fresh on every poll, so the setting has no effect.

Every request to the Shelly has a time limit of 3 s. After a failed Modbus read the connection is dropped and
reopened, so that a late reply can never be mistaken for the answer to the next request.
If a read fails, the old value is **not** sent again.

## Home Assistant offset

Optional. A value read from Home Assistant can be added to the Shelly's power, to move the setpoint that the
inverter regulates around. Import and export sensors are added as virtual import and export: with a virtual export
of 1000 W and a virtual import of 400 W, a net shift of 600 W of export is added.

| Variable | Default | Description |
|---|---|---|
| `HA_URL` | | URL of Home Assistant, e.g. `http://192.168.1.2:8123`. The integration is off if `HA_URL` or `HA_TOKEN` is empty. |
| `HA_TOKEN` | | Long-lived access token. |
| `HA_EXTRA_IMPORT` | | Entity whose value is added as import. |
| `HA_EXTRA_EXPORT` | | Entity whose value is added as export. |
| `HA_SMOOTH` | `false` | Average the offset over the last 10 samples. The average covers the samples there are while the window fills, so there is no dead time after start-up. |
| `HA_OFFSET_PER_PHASE` | `true` | Spread the offset equally over the connected phases (see below). `false` adds it to the total power only. |
| `HA_TIMEOUT_MS` | `2000` | Time limit of one request to Home Assistant (at least 100). |
| `HA_RETRY_S` | `5` | After a failed read, a sensor is not asked again for this long. |
| `HA_MAX_STALE_S` | `30` | How long the last good value of a sensor is kept while it can't be read. After that it counts as 0 W. |

**Per phase.** The inverter does not only look at the total power, it also gets the power, current and power factor
of each phase. With `HA_OFFSET_PER_PHASE=true` the offset is divided equally between the phases that are connected
(voltage above 1 V; a single-phase device gets all of it on its one phase). The reactive power of each phase stays as
measured, apparent power, power factor and current are recalculated from the new active power, so the three phases
always add up to the total. As long as the Shelly has not delivered a complete set of phases, only the total is shifted.

**When Home Assistant has problems.** A sensor counts as failed if the request fails, runs into the time limit,
answers with an error, or if its state is not a number (`unavailable`, `unknown`, empty, `nan`, `inf`).

- The last good value is kept for `HA_MAX_STALE_S` seconds, so a short hiccup doesn't make the offset jump to 0 W.
  Afterwards the sensor counts as 0 W.
- A failed sensor is only asked again every `HA_RETRY_S` seconds. An unreachable Home Assistant therefore costs one
  time limit per retry, not one per cycle, and the power reading from the Shelly is not slowed down.
- With `HA_SMOOTH`, a failed read is never averaged in as 0 W. If all sensors are gone for longer than
  `HA_MAX_STALE_S`, the average starts over when they come back.
- The first failure of a sensor, the moment its value is given up, and its recovery are logged once as warnings
  (`Home Assistant sensor … can't be read`, `… counting it as 0 W`, `… is readable again`), not every cycle.
- A sensor name that is left empty is not used and not requested.

## Logging and failure behaviour

| Variable | Default | Description |
|---|---|---|
| `FRONIUS_DEBUG` | `false` | Verbose logging with timestamps: every Shelly read (raw registers or JSON, and its duration), every value published to the emulated meter, and every request of the inverter with its unit ID, the values it received, the age of the power value and the time since its previous request. Turn it off for normal operation. |

Without the debug flag, warnings are still logged: failed Shelly reads, power reads that take longer than 1 s,
polling cycles that started late, and net power jumps of more than 2 kW between two samples.

**Watchdog:** if no new reading was published for 30 s, the emulator exits with status 1 so that the container is
restarted. The last line names the last Shelly error, for example
`No Raw reading updates in 30s, exiting. Last Shelly error: power read failed: … (12s ago)`.
Set a restart policy (`restart: always`, or "Always"/"Unless stopped" in Portainer). Without one the container stays down,
and the inverter shows the meter as not connected and retries once a minute.

## Examples

Grid meter: Shelly Pro 3EM over Modbus, on the host `192.168.1.240`, registered in the inverter with address 1:

```yaml
services:
  fronius-meter-emulation:
    image: fronius-meter-emulation
    restart: always
    ports:
      - "1502:1502"
    environment:
      - SHELLY_MODBUS=192.168.10.244:502
      - SLOW_METER_READ_INTERVAL_S=5
```

AC battery as production meter: Shelly Pro 1PM over REST, on a second host `192.168.1.194`, registered in the
inverter with address 2. The serial `00000002` follows from the address, nothing else needs to be set:

```yaml
services:
  fronius-meter-emulation:
    image: fronius-meter-emulation
    restart: always
    ports:
      - "1502:1502"
    environment:
      - SHELLY_REST=192.168.10.106
```

Tested with a Fronius Verto Plus, a Shelly Pro 3EM as primary meter, a Shelly Pro 1PM for the AC battery
and a Fronius Ohmpilot.
