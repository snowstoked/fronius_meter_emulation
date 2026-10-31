# Fronius Smart Meter IP Emulation

TL;DR I'm not installing multiple vendors smart meters because of vendor locking.

This software reads a Shelly energy meter (a (Pro) 3EM over Modbus or REST, or a single phase device such as the Pro 1PM over REST)
and provides those readings to a Fronius solar inverter, as if it was a Fronius Smart Meter (Modbus TCP).

Shelly meters only measure about once per second, so the emulated meter is not as fast as a real Fronius Smart Meter and small deviations
from the setpoint remain. This emulator allows zero export control and provides all necessary data to the inverter.

It is also useful in situations where you _can_ export to the grid, but pricing may be non optimal, or where you just want to have the readings
available to the inverter for its app / charts etc.
Such as in Australia when on wholesale pricing and during the middle of the day when export prices go negative.
All this code does is transfer the power readings over between the Shelly and the Fronius Modbus TCP interface.

To enhance this slightly further, biasing values can be read from home assistant (or another http server).
These will be merged in with the "real" readings, to allow software to drive the inverter to regulate around a different setpoint.
This is useful as normally you would set the inverter to some limit X of the maximum grid export amount, and it will internally use a control loop
to try and regulate its output to keep at that setpoint of export.
By using these inputs you can then shift this regulation point to where is desired.
This can be useful when having other controlled loads that are also export power aware.

## Documentation

- [Configuration](docs/configuration.md): every option, the supported Shelly devices (Modbus and REST), running several emulators, failure behaviour.
- [Inverter communication](docs/inverter-communication.md): what the inverter reads from the emulated meter, how often, the identity it expects, latency measurements, and how to capture the traffic yourself.

## Usage

This software is best run as a docker container on a device that has a reliable network connection to all involved devices (i.e avoid WiFi if you can).
Set the source meter (`SHELLY_MODBUS` or `SHELLY_REST`) and register the container's IP address and port (1502 by default) in the inverter as a
"Fronius Smart Meter (Modbus TCP)", with the Modbus address of your choice.

```bash
docker build -t fronius-meter-emulation .
docker run -d --restart always -p 1502:1502 -e SHELLY_MODBUS=192.168.1.5:502 fronius-meter-emulation
```

Use a restart policy. If the Shelly stops answering for 30 s the emulator exits on purpose, and the container has to be restarted for the meter to come back.

### The source meter

- `SHELLY_MODBUS=ip:port`: reads the Shelly over Modbus TCP. Fastest, and what the Pro 3EM is best used with.
- `SHELLY_REST=ip`: reads the Shelly over its HTTP RPC interface, for devices without Modbus. The device type is detected automatically:
  `em:N` components (Pro 3EM, three phases) and `switch:N` components (Pro 1PM and other relays with power metering, reported as one phase).

The power is read twice per second (Shelly devices only update at 1Hz, but this reduces latency). Phases, frequency and energy counters are
refreshed every `SLOW_METER_READ_INTERVAL_S` seconds for Modbus, and with every read for REST.
Every Shelly read has a time limit, and a failed read never re-sends an old value.
See [Configuration](docs/configuration.md) for the details and the list of supported devices.

### Offset via Home Assistant

The Home Assistant controls are read over the API from home assitant at approximately 1Hz.
To aid in control, there are two controls supported; which are added as virtual export and virtual import.
This means if you have a virtual export of 1000W and a virtual import of 400W, a net shift of 600W of export is added to the raw meter
reading before its reported to the virtual meter.

The Home Assistant integration can be configured via the `HA_URL`, `HA_TOKEN`, `HA_EXTRA_IMPORT` and `HA_EXTRA_EXPORT` env vars.

### The Emulated meter

The emulated meter does not implement writing.
It provides the complete measurement block of the Fronius Smart Meter: the inverter reads the total and per-phase values once per second and the
energy counters once per minute. See [Inverter communication](docs/inverter-communication.md) for the measured details.

By default the modbus socket is bound on `0.0.0.0:1502`, but this can be overridden using the `FRONIUS_MODBUS_BIND` env var.
Port 1502 is used as its available in most Fronius firmware's, but you can override it to use a different port if needed.

To use several emulators with one inverter (e.g. a grid meter and an AC battery as production meter), run each one with its own IP address and give each
meter its own Modbus address in the inverter. The serial number that the inverter uses to tell meters apart is taken from that Modbus address
automatically (`00000001`, `00000002`, …), because two meters with the same serial number are not accepted.

### Debugging

Set `FRONIUS_DEBUG=true` to log, with timestamps, every Shelly read, every set of values published to the emulated meter, and every request of the
inverter (with its unit ID, the values it was given and how old the power value was).
Warnings (failed or slow Shelly reads, late polling cycles, sudden >2kW jumps of the net power) are always logged.
If the emulator stops because the Shelly delivered no data for 30 s, the last Shelly error is part of the exit message.

## Configuration

The code is entirely configured by environment variables, to make running in docker easier without having to map volume mounts etc.
All options are described in [docs/configuration.md](docs/configuration.md).

```bash
SHELLY_MODBUS # ip:port of the Shelly's Modbus TCP server
SHELLY_REST # Address of the Shelly for HTTP RPC reads (ip or http://ip), used instead of SHELLY_MODBUS if set
SHELLY_ID # Component id for SHELLY_REST if the device has several (default: lowest; SHELLY_EM_ID is the old name)
SHELLY_INVERT # Flip the direction of power flow (true/false)
SHELLY_POLL_INTERVAL_MS # Interval between power readings in ms (default 500, minimum 100)
SLOW_METER_READ_INTERVAL_S # Interval in seconds between slow meter reads (default 60)
FRONIUS_MODBUS_BIND # IP:port to bind the modbus socket to for emulating the Fronius meter (default 0.0.0.0:1502)
FRONIUS_SERIAL # Serial number announced to the inverter (default: the Modbus address of the request, e.g. 00000002)
FRONIUS_MODBUS_ADDRESS # Device address announced in the identity block (default 240; FRONIUS_DEVICE_ADDRESS is the old name)
FRONIUS_DEBUG # Verbose logging (true/false)
HA_EXTRA_IMPORT # Home Assistant sensor that is added to import readings
HA_EXTRA_EXPORT # Home Assistant sensor that is added to export readings
HA_SMOOTH # Home Assistant smoothing enable disable (true/false)
HA_URL # Home Assistant URL to connect to
HA_TOKEN # Home Assistant token for authentication
```
## Kudos

https://www.photovoltaikforum.com/thread/224214-gen24-smart-meter-modbus-tcp-emulation-mit-esp32/
