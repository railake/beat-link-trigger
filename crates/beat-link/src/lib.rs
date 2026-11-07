//! # beat-link
//!
//! A Rust implementation of the Pioneer Pro DJ Link protocol, ported from the Java
//! [beat-link](https://github.com/Deep-Symmetry/beat-link) library that Beat Link
//! Trigger is built on. It lets a program join a DJ Link network as a virtual player,
//! watch what the real players are doing, retrieve track metadata, beat grids,
//! cues, waveforms and phrase analysis, and act as a tempo master.
//!
//! The main entry point is [`ProDjLink`], which owns and wires together the
//! individual components (device finder, beat finder, virtual CDJ, metadata finder
//! and time finder) that are singletons in the Java implementation.

pub mod beat_finder;
pub mod clock;
pub mod data;
pub mod dbserver;
pub mod device_finder;
pub mod listeners;
pub mod metadata_finder;
pub mod metronome;
pub mod network;
pub mod packets;
pub mod player_settings;
pub mod time_finder;
pub mod util;
pub mod virtual_cdj;

pub use beat_finder::BeatFinder;
pub use device_finder::DeviceFinder;
pub use metadata_finder::{MetadataFinder, TrackData, TrackUpdate};
pub use network::NetworkConfig;
pub use packets::{Beat, CdjStatus, DeviceAnnouncement, DeviceUpdate, MixerStatus, PrecisePosition};
pub use time_finder::TimeFinder;
pub use virtual_cdj::{VirtualCdj, VirtualCdjError, VirtualCdjSettings};

use std::sync::Arc;

/// All the protocol components, wired together.
pub struct ProDjLink {
    pub device_finder: Arc<DeviceFinder>,
    pub beat_finder: Arc<BeatFinder>,
    pub virtual_cdj: Arc<VirtualCdj>,
    pub metadata_finder: Arc<MetadataFinder>,
    pub time_finder: Arc<TimeFinder>,
}

impl ProDjLink {
    /// Build the components with a network configuration (use `Default` for real use).
    pub fn new(config: NetworkConfig) -> Arc<ProDjLink> {
        let device_finder = DeviceFinder::new(config);
        let beat_finder = BeatFinder::new(device_finder.clone());
        let virtual_cdj = VirtualCdj::new(device_finder.clone(), beat_finder.clone());
        let metadata_finder = MetadataFinder::new(device_finder.clone(), virtual_cdj.clone());
        let time_finder = TimeFinder::new(&virtual_cdj, &beat_finder, metadata_finder.clone());
        Arc::new(ProDjLink { device_finder, beat_finder, virtual_cdj, metadata_finder, time_finder })
    }

    /// Go online: start finding devices, join as a virtual CDJ, and start the
    /// beat, metadata and time finders. Blocks until a device number is claimed.
    pub fn go_online(&self, settings: VirtualCdjSettings) -> Result<(), VirtualCdjError> {
        self.virtual_cdj.configure(settings);
        self.device_finder.start()?;
        self.virtual_cdj.start()?;
        self.beat_finder.start()?;
        self.metadata_finder.start();
        self.time_finder.start();
        Ok(())
    }

    /// Go offline, stopping everything.
    pub fn go_offline(&self) {
        self.time_finder.stop();
        self.metadata_finder.stop();
        self.virtual_cdj.stop();
        self.beat_finder.stop();
        self.device_finder.stop();
    }

    /// Whether we are online.
    pub fn is_online(&self) -> bool {
        self.virtual_cdj.is_running()
    }
}
