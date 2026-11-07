//! End-to-end tests on the loopback interface: a fake CDJ at 127.0.0.2 announces
//! itself, sends status packets and serves metadata from a fake dbserver, while the
//! real protocol stack joins as a virtual CDJ at 127.0.0.1.
#![cfg(target_os = "linux")]

use beat_link::dbserver::{kind, Field, Message, DB_SERVER_QUERY_PORT};
use beat_link::network::{InterfaceAddress, NetworkConfig, Ports};
use beat_link::packets::{CdjStatusBuilder, TrackSourceSlot, TrackType};
use beat_link::util::{self, PacketType};
use beat_link::{ProDjLink, VirtualCdjSettings};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const PLAYER: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const US: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

fn keep_alive(number: u8, name: &str) -> Vec<u8> {
    let mut p = vec![0u8; 0x36];
    p[..10].copy_from_slice(&util::MAGIC_HEADER);
    p[0x0a] = PacketType::DeviceKeepAlive.protocol_value();
    p[0x0c..0x0c + name.len()].copy_from_slice(name.as_bytes());
    p[0x20] = 0x01;
    p[0x21] = 0x02;
    p[0x23] = 0x36;
    p[0x24] = number;
    p[0x2c..0x30].copy_from_slice(&PLAYER.octets());
    p
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn menu_item(item_type: u32, id: u32, label: &str) -> Vec<Field> {
    vec![
        Field::number(0),
        Field::number(id),
        Field::number(0),
        Field::String(label.into()),
        Field::number(0),
        Field::String(String::new()),
        Field::number(item_type),
        Field::number(0),
        Field::number(0),
        Field::number(0),
        Field::number(0),
        Field::number(0),
    ]
}

/// A minimal dbserver that serves one track's metadata and beat grid.
fn run_fake_dbserver(listener: TcpListener, player: u8) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            thread::spawn(move || {
                let greeting = Field::read(&mut s).unwrap();
                assert_eq!(greeting.as_number(), Some(1));
                s.write_all(&Field::number(1).to_bytes()).unwrap();
                let mut pending_items: Vec<Vec<Field>> = Vec::new();
                while let Ok(m) = Message::read(&mut s) {
                    let reply = |kind: u16, args: Vec<Field>| Message::new(m.transaction, kind, args).to_bytes();
                    match m.kind {
                        kind::SETUP_REQ => {
                            s.write_all(&reply(kind::MENU_AVAILABLE, vec![Field::number(0), Field::number(u32::from(player))])).unwrap()
                        }
                        kind::TEARDOWN_REQ => break,
                        kind::REKORDBOX_METADATA_REQ => {
                            pending_items = vec![
                                menu_item(beat_link::dbserver::item::TRACK_TITLE, 42, "Loopback Anthem"),
                                menu_item(beat_link::dbserver::item::ARTIST, 7, "The Testers"),
                                menu_item(beat_link::dbserver::item::DURATION, 240, ""),
                                menu_item(beat_link::dbserver::item::TEMPO, 12800, ""),
                            ];
                            s.write_all(&reply(kind::MENU_AVAILABLE, vec![Field::number(u32::from(m.kind)), Field::number(pending_items.len() as u32)]))
                                .unwrap();
                        }
                        kind::RENDER_MENU_REQ => {
                            let mut out = reply(kind::MENU_HEADER, vec![]);
                            for args in &pending_items {
                                out.extend(Message::new(m.transaction, kind::MENU_ITEM, args.clone()).to_bytes());
                            }
                            out.extend(Message::new(m.transaction, kind::MENU_FOOTER, vec![]).to_bytes());
                            s.write_all(&out).unwrap();
                        }
                        kind::BEAT_GRID_REQ => {
                            let mut blob = vec![0u8; 20];
                            for i in 0..16u32 {
                                let mut e = [0u8; 16];
                                e[0..2].copy_from_slice(&((i % 4 + 1) as u16).to_le_bytes());
                                e[2..4].copy_from_slice(&12800u16.to_le_bytes());
                                e[4..8].copy_from_slice(&(100 + i * 469).to_le_bytes());
                                blob.extend_from_slice(&e);
                            }
                            s.write_all(&reply(
                                kind::BEAT_GRID,
                                vec![Field::number(u32::from(m.kind)), Field::number(0), Field::number(blob.len() as u32), Field::Binary(blob), Field::number(0)],
                            ))
                            .unwrap();
                        }
                        _ => s.write_all(&reply(kind::UNAVAILABLE, vec![Field::number(u32::from(m.kind))])).unwrap(),
                    }
                }
            });
        }
    });
}

#[test]
fn joins_network_receives_status_and_fetches_metadata() {
    let _ = env_logger_init();
    let ports = Ports { announcement: 47000, beat: 47001, update: 47002 };
    let config = NetworkConfig {
        ports,
        bind_address: Ipv4Addr::UNSPECIFIED,
        forced_interface: Some(InterfaceAddress {
            interface_name: "lo".into(),
            address: US,
            prefix_length: 8,
            broadcast: US,
            hardware_address: Some([2, 0, 0, 0, 0, 1]),
        }),
    };
    let link = ProDjLink::new(config);

    // The fake player's dbserver: port discovery on 12523 and the server itself.
    let query = TcpListener::bind(SocketAddrV4::new(PLAYER, DB_SERVER_QUERY_PORT)).expect("bind 127.0.0.2:12523");
    let db = TcpListener::bind(SocketAddrV4::new(PLAYER, 0)).unwrap();
    let db_port = db.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in query.incoming() {
            let Ok(mut s) = s else { break };
            let mut buf = [0u8; 19];
            if s.read_exact(&mut buf).is_ok() {
                let _ = s.write_all(&db_port.to_be_bytes());
            }
        }
    });
    run_fake_dbserver(db, 2);

    // The fake player announces itself and sends status.
    let player_socket = UdpSocket::bind(SocketAddrV4::new(PLAYER, 0)).unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    thread::spawn(move || {
        let mut packet_number = 0;
        while r.load(Ordering::SeqCst) {
            let _ = player_socket.send_to(&keep_alive(2, "CDJ-3000"), SocketAddrV4::new(US, ports.announcement));
            packet_number += 1;
            let status = CdjStatusBuilder {
                device_number: 2,
                track_source_player: 2,
                track_source_slot: TrackSourceSlot::UsbSlot,
                track_type: TrackType::Rekordbox,
                rekordbox_id: 42,
                bpm: 12800,
                beat_number: Some(3),
                beat_within_bar: 3,
                playing: true,
                master: true,
                on_air: true,
                packet_number,
                ..Default::default()
            };
            let _ = player_socket.send_to(&status.build_bytes(), SocketAddrV4::new(US, ports.update));
            thread::sleep(Duration::from_millis(100));
        }
    });

    // Listen to what the virtual CDJ announces on the player's side.
    let (tx, rx) = mpsc::channel();
    link.metadata_finder.track_update.add(move |u| {
        if let Some(d) = &u.data {
            let _ = tx.send((u.player, d.clone()));
        }
    });

    let settings = VirtualCdjSettings {
        device_name: "Rust Trigger".into(),
        device_number: 0,
        claim_interval: Duration::from_millis(20),
        self_assignment_watch_period: Duration::from_millis(200),
        device_wait: Duration::from_secs(5),
        announce_interval_ms: 200,
        ..Default::default()
    };
    link.go_online(settings).expect("went online");
    assert!(link.is_online());
    let number = link.virtual_cdj.device_number();
    assert!((7..16).contains(&number), "self-assigned a non-player number, got {number}");
    assert_eq!(link.virtual_cdj.device_name(), "Rust Trigger");

    // Status from the fake player arrives, and it is recognized as tempo master.
    assert!(wait_for(Duration::from_secs(3), || link.virtual_cdj.latest_cdj_status(2).is_some()));
    let status = link.virtual_cdj.latest_cdj_status(2).unwrap();
    assert!(status.is_playing());
    assert_eq!(status.rekordbox_id, 42);
    assert!(wait_for(Duration::from_secs(2), || link.virtual_cdj.tempo_master().is_some()));
    assert!((link.virtual_cdj.master_tempo() - 128.0).abs() < 0.01);

    // Metadata and beat grid arrive from the fake dbserver.
    let (player, data) = rx.recv_timeout(Duration::from_secs(10)).expect("metadata delivered");
    assert_eq!(player, 2);
    let md = data.metadata.as_ref().unwrap();
    assert_eq!(md.title, "Loopback Anthem");
    assert_eq!(md.artist_label(), "The Testers");
    assert_eq!(md.duration, 240);
    let grid = data.beat_grid.as_ref().expect("beat grid");
    assert_eq!(grid.beat_count(), 16);

    // With a beat grid, the time finder can place the playhead.
    assert!(wait_for(Duration::from_secs(3), || link.time_finder.time_for(2).is_some()));
    let t = link.time_finder.time_for(2).unwrap();
    assert!(t >= 1038, "position {t} should be at or after beat 3");

    running.store(false, Ordering::SeqCst);
    link.go_offline();
    assert!(!link.is_online());
}

fn env_logger_init() -> Result<(), ()> {
    Ok(())
}
