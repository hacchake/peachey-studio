//! CLAP plugin hosting for Peel Studio.
//!
//! A [`ClapDevice`] wraps one CLAP plugin as a [`peel_dsp::Device`], so the
//! engine drives it in a device slot exactly like a native instrument or
//! effect. Plugins are found with [`scan`] (or, safely, [`scan_isolated`]) and
//! described by [`ClapPluginInfo`].
//!
//! # Using it
//!
//! 1. **Scan off the UI thread.** Run [`scan_isolated`] with the
//!    `peel-clap-scan` helper ([`default_scanner_path`]) at startup or on
//!    demand, cache the [`ClapPluginInfo`] list, and show it in the browser.
//!    [`scan`] does the same in-process and is only appropriate for tools:
//!    a crashing plugin would take the process with it.
//! 2. **Load on a control thread.** [`ClapDevice::load`] creates the plugin by
//!    id, builds a `&'static DeviceDescriptor` (type id `clap:<plugin id>`,
//!    parameter ids and keys taken from the plugin's own stable ids), and
//!    activates it.
//! 3. **Keep a handle, install the device.** Take [`ClapDevice::handle`], then
//!    box the device and send it to the engine like any `Box<dyn Device>`.
//! 4. **Call [`ClapHandle::idle`] every UI frame.** Plugins ask the host to run
//!    things on the main thread; `idle` is where that happens.
//! 5. **Save state with [`ClapHandle::save_state`]** into the project and give
//!    it back with [`ClapHandle::load_state`] (or [`ClapDevice::load_state`]
//!    before installing the device). The blob is opaque and only the plugin
//!    that wrote it can read it.
//!
//! # Threads
//!
//! CLAP names two thread roles and lets them run at once on one instance.
//!
//! | Role | What runs there here | Rules |
//! | --- | --- | --- |
//! | main | `load`, `prepare`, `idle`, state, parameter text, scanning | may block and allocate; serialised per plugin by a lock |
//! | audio | `process`, `set_param`, `param`, `reset`, `is_silent`, `latency`, `tail_seconds` | no allocation, no locks, no I/O (`docs/REALTIME.md`) |
//!
//! The `clap.thread-check` extension answers truthfully for the calling
//! thread. Plugin callbacks into the host (`request_restart`,
//! `request_process`, `request_callback`, parameter rescan and flush, latency
//! and tail changes, `mark_dirty`) only set atomic flags, so they are safe
//! from any thread. `idle` collects them and returns an [`IdleReport`].
//!
//! `prepare` deactivates and activates the plugin and so must not run while
//! the engine owns the device. That fits the engine's contract: a device is
//! prepared before it is installed, and a changed rate or block size means
//! installing a prepared replacement.
//!
//! # Audio
//!
//! The plugin's main audio ports map onto the `Device` stereo pair (see the
//! `audio` module for mono and multichannel mapping). Effects get the pair as
//! input and replace it; instruments ignore the incoming buffers and
//! overwrite them. Plugins that process in blocks larger than the engine's
//! are never asked to: a block longer than `max_block` is split.
//!
//! # Parameters
//!
//! `set_param` updates the parameter mirror (`param` and
//! [`ClapHandle::param`] read it) and delivers the value to the plugin. While
//! the device is processing, the value is queued (the latest wins per
//! parameter, so the queue cannot overflow) and the next `process` sends it as
//! a CLAP parameter event at frame 0. Before the first block and while the
//! plugin is asleep (`is_silent`), it goes straight through `params.flush`
//! instead, so a knob moves and `save_state` is current even when the
//! transport is stopped. `EventKind::Param` events are delivered at their
//! sample offset.
//!
//! Stepped parameters starting at zero with at most 64 values become choices
//! labelled by the plugin's own `value_to_text`; hidden and read-only
//! parameters are not exposed. Restore a saved state with `load_state`
//! *before* replaying stored parameter values, and do not call
//! `peel_dsp::device::apply_defaults` afterwards: for a plugin the state is
//! the source of truth and defaults would overwrite it.
//!
//! # Not covered yet
//!
//! Plugin GUIs outside Windows (on Windows, [`ClapHandle::open_editor`] opens
//! the plugin's window embedded in a host window), parameter
//! modulation, note expressions, MPE/MIDI 2.0 dialects, auxiliary and
//! multi-output routing, presets, remote controls, and the thread pool and
//! timer extensions.

#[cfg(test)]
extern crate self as peel_host_clap;

mod audio;
mod descriptor;
mod device;
mod error;
mod events;
mod gui;
mod handle;
mod host;
mod info;
mod instance;
mod isolated;
mod module;
mod params;
mod scan;
mod scoped;
mod state;
mod thread;

pub use descriptor::cached_descriptor;
pub use device::ClapDevice;
pub use error::ClapError;
pub use handle::{ClapHandle, ClapStats};
pub use info::{ClapPluginInfo, ClapRole};
pub use instance::IdleReport;
pub use isolated::{default_scanner_path, run_scan_worker, scan_file_isolated, scan_isolated};
pub use scan::{
    ScanFailure, ScanReport, candidate_files, scan, scan_file, scan_paths, search_paths,
};
