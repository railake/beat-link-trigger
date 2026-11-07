# REA evidence for the Rust rewrite

[REA](https://github.com/morluto/rea) (Reverse Engineer Anything, `rea-agents`
6.8.0) was used to inventory the original sources and to observe the rewrite's
actual behavior: its shipped binary, its process behavior, and its network
traffic. The Evidence files in [`evidence/`](evidence/) are REA's unmodified JSON
output (their `evidence_id`s are content commitments, so they have not been
edited; they contain the absolute paths of the machine they were captured on).

The Java/Clojure original could not be built in the capture environment (no
Leiningen or Maven repository access), so no authority-vs-reconstruction process
comparison was made. The protocol behavior was instead checked against the
beat-link source it was ported from.

## 1. Inventory of the original (`blt-clojure-reference-summary.json`)

`rea import-reference-source src --detail summary` recorded the 29 Clojure files
of the original application (1.3 MB) as historical reference, with their digests
and the git commit they came from (`4ee17f8`). REA does not parse Clojure, so this
is an inventory, not a semantic graph; the parity ledger in
[`RUST-REWRITE.md`](../../RUST-REWRITE.md) maps each of those namespaces to its
Rust replacement.

## 2. Protocol on the wire (`prodjlink-loopback.pcapng`, `pcap-summary.json`, `packets.json`)

The loopback integration test (`crates/beat-link/tests/loopback.rs`) was run while
`dumpcap` recorded the loopback interface. `rea inspect-pcap-capture` summarized 64
packets between the Rust virtual CDJ (127.0.0.1) and the fake player (127.0.0.2),
and `rea inspect-packet-capture` extracted the UDP payloads. Grouped by sender,
destination port, length and packet type byte (offset 0x0a), every payload starts
with the Pro DJ Link header `Qspt1WmJOL`:

| Count | From | Port role | Bytes | Type | Meaning |
| ---: | --- | --- | ---: | --- | --- |
| 3 | virtual CDJ | announcement | 38 | 0x0a | hello |
| 3 | virtual CDJ | announcement | 44 | 0x00 | device number claim stage 1 |
| 3 | virtual CDJ | announcement | 50 | 0x02 | claim stage 2 |
| 3 | virtual CDJ | announcement | 38 | 0x04 | claim stage 3 |
| 1 | virtual CDJ | announcement | 54 | 0x06 | keep-alive |
| 5 | fake player | announcement | 54 | 0x06 | keep-alive |
| 5 | fake player | update | 512 | 0x0a | CDJ status |

This is the same sequence and the same packet sizes that beat-link's
`VirtualCdj.claimDeviceNumber()` produces (three hellos, then three packets of
each claim stage, then keep-alives). The remaining 41 packets are the TCP
dbserver exchange (port query, greeting, setup, metadata menu, render, beat grid
and teardown).

## 3. Process behavior (`scenario-*.json`, `capture-*.json`)

`rea capture-process` ran the release binary twice under a PTY:

* **Convert** (`--convert BreakBuddy.bls --output out/BreakBuddy.blsj`): exited 0,
  printed the conversion message, and created exactly one file,
  `out/BreakBuddy.blsj` (19,688 bytes, SHA-256 `7ee6e4a9…`). REA verified that the
  owned process group was cleaned up.
* **Headless simulation** (`--headless --offline --simulate 2 --run-for 3`): exited
  0 after three seconds. The log shows simulated players 1 and 2 present, a
  trigger activating and deactivating (MIDI sending failed because the capture
  host has no ALSA sequencer device), and a clean shutdown. The filesystem
  effects are the preferences file (`preferences.bltj`) and the log file
  (`logs/blt.log`) in the redirected home directory.

REA lists four residual unknowns for each capture: process trees are sampled,
the executable digest is a pre-launch sample, the inherited environment is not
recorded, and network activity is not observed.

## 4. The shipped binary (`rust-binary.json`)

The release build was compiled with `cargo auditable`, and `rea inspect-rust-binary`
read its embedded dependency manifest: 331 packages with 868 dependency edges,
rooted at `beat-link-trigger 9.0.0-alpha.1`, with the local `beat-link` protocol
crate and, among others, `rhai 1.27.0` (expressions), `eframe 0.31.1` (GUI),
`midir 0.10.4` (MIDI), `rosc 0.10.1` (OSC), `minijinja 2.24.0` (overlay templates)
and `tiny_http 0.12.0` (overlay server). No JVM or Clojure component is present.

## Reproducing

```bash
npm install --global rea-agents@6.8.0
```

```bash
cargo install cargo-auditable --locked && cargo auditable build --release -p beat-link-trigger
```

```bash
rea inspect-rust-binary target/release/beat-link-trigger --format json
```

```bash
rea capture-process docs/rust-rewrite/evidence/scenario-convert.json --format json
```

The scenario files hold absolute paths from the capture machine; edit the
`executable`, `working_directory` and input paths before reusing them. Packet
inspection needs `tshark` and `capinfos` on the `PATH`.
