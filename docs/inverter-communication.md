# How the inverter talks to the emulated meter

This describes what a Fronius inverter actually sends to the emulator, what the emulator answers, and how long the
data takes to get there. It was measured, not read from a specification, so it is only as general as the setup it
was measured on (see [Measurement setup](#measurement-setup)). Things that are inferred rather than observed are
marked as such.

## Measurement setup

| | |
|---|---|
| Inverter | Fronius Verto Plus (one device, one firmware: other models may behave differently) |
| Primary meter | Shelly Pro 3EM, read over Modbus, registered in the inverter with Modbus address 1 |
| Production meter ("AC battery") | Shelly Pro 1PM, read over REST, registered with Modbus address 2, on a second emulator with its own IP |
| Other | Fronius Ohmpilot |
| Capture | `tcpdump` on the host of the second emulator, 206 s of normal operation, 427 request/reply pairs |
| Logs | `FRONIUS_DEBUG=true` logs of both emulators, 60 to 1,430 s each |

## The connection

- The inverter is the TCP client and connects to the emulator on the registered IP and port (1502 by default).
  It keeps **two long-lived connections** open per meter (two client ports were seen for the whole capture).
- It only uses **function code 3, Read Holding Registers**. No writes, no other function codes, and no exception
  replies in the working capture.
- The **unit ID** in every request is the Modbus address configured for that meter in the inverter. All 427
  requests of the second meter carried unit ID 2.
- The emulator answers every unit ID with the same data. The only thing that depends on the unit ID is the serial
  number (see [Identity](#identity-block)).
- Two meters cannot share an IP address in the inverter, so every emulator needs its own IP address
  (own host, container with its own address, or macvlan).

## Discovery (once, after connecting)

The inverter first checks what is behind the address. This is the whole sequence seen on a working connection:

| Registers | What it is | Answer |
|---|---|---|
| `768/1` (3 times), `1706/1` (6 times) | Probes for other, non-SunSpec devices | `0`, which means "not that device" |
| `40000/2` | SunSpec marker | `0x5375 0x6e53` = "SunS" |
| `40002/2` | Model ID and length of the common block | `1`, `65` |
| `40069/2` | Header of the next model | `213`, `124` (three-phase meter, float) |
| `40195/2` | Header after that model | `0xFFFF`, `0` = end of map |
| `40004/66` | The whole common block (manufacturer, model, serial, address) | see below |

If a read fails, the inverter marks the meter as not connected. It retries once a minute: with the emulator
not listening, a burst of about 13 refused connection attempts was seen every 60 s, for the full hour of that capture.

In the one failing case that was recorded (two emulators announcing the same serial number), the sequence ended
after the identity read with three reads of register `11/1`, and the meter stayed red. Those reads were not
seen on a working connection. What they are for is not known.

## Steady state

| Registers | Interval | Content |
|---|---|---|
| `40071/58` | **1.00 s** (205 reads in 206 s, no jitter) | The measurement block of the meter model: currents, voltages, frequency, power, VA, VAR, power factor |
| `40193/2` | 1.00 s, together with the one above | The end-of-map header (`0xFFFF`, `0`). Inferred: used to notice that the map changed |
| `40129/32` | **every 60 s** (at 60, 120 and 180 s) | The energy counters: export at `40129`, import at `40137`, as floats in Wh |

So the inverter does use the per-phase values and the energy counters. With an earlier version that only filled in the
total power, the inverter did not use the battery as needed (see the README for the tested setup).

Register map of the measurement block, all 32 bit floats (two registers, high word first):

| Register | Value | Register | Value |
|---|---|---|---|
| `40071` | Total current | `40105` | Total VA |
| `40073`, `40075`, `40077` | Current A, B, C | `40107`, `40109`, `40111` | VA A, B, C |
| `40079` | Average phase voltage | `40113` | Total VAR |
| `40081`, `40083`, `40085` | Voltage A, B, C | `40115`, `40117`, `40119` | VAR A, B, C |
| `40087` | Average line voltage | `40121` | Total power factor |
| `40089`, `40091`, `40093` | Voltage AB, BC, CA | `40123`, `40125`, `40127` | Power factor A, B, C |
| `40095` | Frequency | `40129` | Energy export (Wh) |
| `40097` | **Total active power (W)** | `40137` | Energy import (Wh) |
| `40099`, `40101`, `40103` | Active power A, B, C | | |

Positive total power means import from the grid, as on a Fronius Smart Meter at the grid connection point.

### Sign on a production meter

In the capture the emulator sent **+5.8 W** (the 1PM measured its load consuming a little standby power), and the
inverter showed **-5.70 W** for that meter, which is registered as "Production meter". So the inverter appears to show
consumption on a production meter as negative, which is the physically correct way round. This is one data point
from a standby load, not a check while the batteries discharge, so confirm the sign during a discharge before
deciding whether `SHELLY_INVERT` is needed.

## Identity block

The common block (`40002` to `40069`) tells the inverter what it is talking to. The emulator announces:

| Register | Content | Value |
|---|---|---|
| `40004`–`40019` | Manufacturer | `Fronius` |
| `40020`–`40035` | Model | `Smart Meter 63A` |
| `40052`–`40067` | **Serial number**, one ASCII character per register, zero padded | the Modbus address with leading zeros, e.g. `00000002` |
| `40068` | Device address | `240` (`FRONIUS_MODBUS_ADDRESS` overrides it) |
| `40069`, `40070` | Next model | `213`, `124` |

**Every meter needs its own serial number.** Two emulators that both announced `00000001` left the second meter red
(`N/A`) in the inverter. After giving it a different serial it turned green on the next attempt. That is the
observation. That the cause is the duplicate serial is an inference, as the inverter gives no reason; nothing else
differed between the two emulators except IP, address and the measured values.

The device address in the identity block does not seem to be checked: the primary meter works with address 1 in
the inverter while the emulator announces 240.

## Latency

Where the time goes between the Shelly measuring and the inverter reading the value:

```
Shelly measures (about 1 Hz) -> emulator polls the Shelly (500 ms) -> value in the register file -> inverter reads it (1 s)
```

| Step | Measured |
|---|---|
| Emulator reply to an inverter request | median **0.13 ms**, 95th percentile 0.48 ms, maximum 2.6 ms (capture, 427 pairs, on the emulator host) |
| Reading the Shelly over **Modbus** (Pro 3EM) | median **1 ms**, slowest 220 ms and 405 ms in the two logs (the first read after connecting took about 300 ms) |
| Reading the Shelly over **REST** (Pro 3EM) | median **55 ms**; 2 of 124 reads took about 1.1 s, one 0.63 s |
| Reading the Shelly over **REST** (Pro 1PM) | 109 ms in the one logged sample; 5 of about 2,850 reads (0.2 %) took 1.09 to 1.19 s |
| **Age of the power value when the inverter read it**, Modbus | median **90 ms**, 95th percentile 510 ms, maximum 636 ms |
| Age of the power value when the inverter read it, REST (Pro 3EM) | median 187 ms, 95th percentile 606 ms, maximum 1,048 ms |

What this means:

- The emulator itself adds almost nothing. The delay is in the Shelly (it only produces a new measurement about
  once a second), the 500 ms poll interval and the inverter's own 1 s poll.
- **Estimate, not measured:** adding the parts gives a worst case of about 2.5 to 3 s from a change at the Shelly
  to the inverter acting on it (about 1 s Shelly update + 0.5 s poll + read time + 1 s inverter poll), and roughly
  half of that on average.
- Modbus is the better source for the inverter's main meter: faster reads and no stalls in the logs. REST is
  needed for devices without Modbus (Pro 1PM).
- The roughly 1.1 s REST stalls are suspiciously uniform, which points to a fixed timer such as a lost first TCP
  packet being retransmitted after 1 s, not to the Shelly being slow. This was not confirmed. A check from the host:
  `curl -s -o /dev/null -w "%{time_connect} %{time_total}\n" http://<shelly>/rpc/Switch.GetStatus?id=0` in a loop,
  looking at calls over 0.5 s.
- With **`SHELLY_POLL_INTERVAL_MS`** below 500 the emulator asks more often, but the Shelly still measures at about
  1 Hz, so the gain is small.

Compared with the original PR code: the slow reads (phases, energy) ran in the same loop as the power read, so
the power value froze while they ran. In a simulation with a Shelly that needs 0.25 s per Modbus request this was
a freeze of 1.5 s every slow cycle; now the slow reads are spread over the fast cycles and the worst case was 0.76 s.
Updates are also written to the register file as one batch, so the inverter can never read half old, half new values.

## Capturing the traffic yourself

On the host that runs the emulator:

```bash
sudo timeout 70 tcpdump -i any -w modbus.pcap tcp port 1502
```

- Capture at least 70 s so that one 60 s energy read is included, and start it while the inverter is connected.
- With Docker, `-i any` shows every packet twice (host side and container bridge). Count only the packets to the host's IP.
- Decoding with Wireshark: Analyze, Decode As, TCP port 1502, Modbus/TCP, then filter `mbtcp`. On the command line:

```bash
tshark -r modbus.pcap -d tcp.port==1502,mbtcp -Y mbtcp \
  -T fields -e frame.time_relative -e ip.src -e ip.dst \
  -e mbtcp.unit_id -e mbtcp.func_code -e mbtcp.reference_num -e mbtcp.word_cnt
```

An empty capture file (24 bytes) means that nothing reached port 1502 on that host. A capture with only SYN and RST
packets means that the connections were refused, i.e. the emulator was not running.

## Not verified

- Other inverters or firmware versions, and what the inverter does when it has several meters of the same kind.
- Whether the inverter really rejects duplicate serial numbers or only dislikes something else that two identical
  emulators share.
- The purpose of the `11/1`, `768/1` and `1706/1` reads.
- The sign of a production meter while the batteries discharge.
