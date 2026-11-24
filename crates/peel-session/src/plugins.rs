//! Third-party plugins (CLAP and VST3): discovery, instances, saved state
//! and plugin windows.
//!
//! Scanning runs on a background thread, in a separate process per plugin
//! file when the `peel-clap-scan` / `peel-vst3-scan` helpers sit next to the
//! executable (so a crashing plugin cannot take the app down), otherwise in
//! process. Instances are created on the control thread, restored from their
//! saved state, and wrapped as ordinary [`peel_dsp::Device`]s for the engine.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use base64::Engine as _;
use peel_document::{DeviceInstance, Id};
use peel_dsp::{Device, ParamId};
use peel_host_clap::{ClapDevice, ClapHandle, ClapPluginInfo, ClapRole};
use peel_host_vst3::{Vst3Device, Vst3Handle, Vst3PluginInfo};

/// Which plugin standard a plugin uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginFormat {
    Clap,
    Vst3,
}

/// One installed plugin, whatever its format.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInfo {
    /// Device type id stored in projects: `clap:<id>` or `vst3:<class id>`.
    pub type_id: String,
    pub name: String,
    pub vendor: String,
    pub description: String,
    pub format: PluginFormat,
    instrument: bool,
    effect: bool,
    hostable: bool,
}

impl PluginInfo {
    pub fn is_instrument(&self) -> bool {
        self.instrument
    }

    pub fn is_effect(&self) -> bool {
        self.effect
    }

    pub fn is_hostable(&self) -> bool {
        self.hostable
    }

    fn from_clap(p: &ClapPluginInfo) -> PluginInfo {
        PluginInfo {
            type_id: format!("clap:{}", p.id),
            name: p.name.clone(),
            vendor: p.vendor.clone(),
            description: p.description.clone(),
            format: PluginFormat::Clap,
            instrument: p.is_instrument(),
            effect: p.is_effect(),
            hostable: p.is_hostable(),
        }
    }

    fn from_vst3(p: &Vst3PluginInfo) -> PluginInfo {
        PluginInfo {
            type_id: p.type_id(),
            name: p.name.clone(),
            vendor: p.vendor.clone(),
            description: if p.sub_categories.is_empty() {
                "VST3".into()
            } else {
                format!("VST3 · {}", p.sub_categories.replace('|', " / "))
            },
            format: PluginFormat::Vst3,
            instrument: p.is_instrument(),
            effect: p.is_effect(),
            hostable: p.is_hostable(),
        }
    }
}

/// Device type id for a plugin.
pub fn type_id_for(info: &PluginInfo) -> String {
    info.type_id.clone()
}

/// True for a device type that is a third-party plugin.
pub fn is_plugin_type(type_id: &str) -> bool {
    type_id.starts_with("clap:") || type_id.starts_with("vst3:")
}

/// A live plugin instance, seen from the main thread.
#[derive(Clone)]
pub enum PluginHandle {
    Clap(Box<ClapHandle>),
    Vst3(Vst3Handle),
}

impl PluginHandle {
    /// Main-thread housekeeping. True when the plugin asks to be rebuilt.
    pub fn idle(&self) -> bool {
        match self {
            PluginHandle::Clap(h) => h.idle().restart_requested,
            PluginHandle::Vst3(h) => h.idle().restart_requested,
        }
    }

    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        match self {
            PluginHandle::Clap(h) => h.save_state().map_err(|e| e.to_string()),
            PluginHandle::Vst3(h) => h.save_state(),
        }
    }

    pub fn param(&self, id: ParamId) -> Option<f32> {
        match self {
            PluginHandle::Clap(h) => h.param(id),
            PluginHandle::Vst3(h) => h.param(id),
        }
    }

    /// The value last set from the plugin's own window, if that window (not
    /// the host) changed it last.
    pub fn edited_in_window(&self, id: ParamId) -> Option<f32> {
        match self {
            PluginHandle::Clap(_) => None,
            PluginHandle::Vst3(h) => h.edited_in_window(id),
        }
    }

    pub fn param_text(&self, id: ParamId, value: f32) -> Option<String> {
        match self {
            PluginHandle::Clap(h) => h.param_text(id, value),
            PluginHandle::Vst3(h) => h.param_text(id, value),
        }
    }

    /// Whether the plugin offers its own window (Windows only for now).
    pub fn has_editor(&self) -> bool {
        match self {
            PluginHandle::Clap(h) => h.has_editor(),
            PluginHandle::Vst3(h) => h.has_editor(),
        }
    }

    pub fn editor_open(&self) -> bool {
        match self {
            PluginHandle::Clap(h) => h.editor_open(),
            PluginHandle::Vst3(h) => h.editor_open(),
        }
    }

    pub fn open_editor(&self) -> Result<(), String> {
        match self {
            PluginHandle::Clap(h) => h.open_editor(),
            PluginHandle::Vst3(h) => h.open_editor(),
        }
    }

    pub fn close_editor(&self) {
        match self {
            PluginHandle::Clap(h) => h.close_editor(),
            PluginHandle::Vst3(h) => h.close_editor(),
        }
    }
}

/// What one background scan found.
#[derive(Default)]
struct Found {
    clap: Vec<ClapPluginInfo>,
    vst3: Vec<Vst3PluginInfo>,
}

#[derive(Default)]
pub struct PluginRegistry {
    rx: Option<Receiver<Found>>,
    clap: Vec<ClapPluginInfo>,
    vst3: Vec<Vst3PluginInfo>,
    list: Vec<PluginInfo>,
    scanned: bool,
    handles: HashMap<Id, PluginHandle>,
    /// Instances that failed to load; not retried until the next scan.
    failed: HashSet<Id>,
}

fn decode_state(name: &str, state: &Option<String>) -> Option<Vec<u8>> {
    let state = state.as_ref()?;
    match base64::engine::general_purpose::STANDARD.decode(state) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            log::warn!("{name}: corrupt saved state: {e}");
            None
        }
    }
}

impl PluginRegistry {
    /// Starts scanning the standard plugin folders in the background.
    pub fn start_scan() -> PluginRegistry {
        let mut r = PluginRegistry::default();
        r.rescan();
        r
    }

    /// Scans again in the background; live instances are unaffected.
    pub fn rescan(&mut self) {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("peel-plugin-scan".into())
            .spawn(move || {
                let clap = match peel_host_clap::default_scanner_path() {
                    Some(scanner) => {
                        peel_host_clap::scan_isolated(
                            &scanner,
                            &peel_host_clap::search_paths(),
                            Duration::from_secs(15),
                        )
                        .plugins
                    }
                    None => peel_host_clap::scan(),
                };
                let vst3 = match peel_host_vst3::default_scanner_path() {
                    Some(scanner) => {
                        peel_host_vst3::scan_isolated(
                            &scanner,
                            &peel_host_vst3::search_paths(),
                            Duration::from_secs(30),
                        )
                        .plugins
                    }
                    None => peel_host_vst3::scan(),
                };
                let _ = tx.send(Found { clap, vst3 });
            })
            .ok();
        self.rx = Some(rx);
        self.scanned = false;
    }

    /// Uses `plugins` as the installed CLAP set (tests, or a cached scan).
    pub fn set(&mut self, plugins: Vec<ClapPluginInfo>) {
        self.rx = None;
        self.accept(Found {
            clap: plugins,
            vst3: Vec::new(),
        });
    }

    fn accept(&mut self, found: Found) {
        let Found { mut clap, mut vst3 } = found;
        clap.retain(|p| matches!(p.role, ClapRole::Instrument | ClapRole::AudioEffect));
        clap.sort_by_key(|p| p.name.to_lowercase());
        clap.dedup_by(|a, b| a.id == b.id);
        vst3.retain(|p| p.is_hostable());
        vst3.sort_by_key(|p| p.name.to_lowercase());
        vst3.dedup_by(|a, b| a.cid == b.cid);
        log::info!("found {} CLAP and {} VST3 plugins", clap.len(), vst3.len());
        let mut list: Vec<PluginInfo> = clap
            .iter()
            .map(PluginInfo::from_clap)
            .chain(vst3.iter().map(PluginInfo::from_vst3))
            .collect();
        list.sort_by_key(|p| p.name.to_lowercase());
        self.clap = clap;
        self.vst3 = vst3;
        self.list = list;
        self.scanned = true;
        self.failed.clear();
    }

    /// An empty registry that never scans (tests, headless tools).
    pub fn disabled() -> PluginRegistry {
        PluginRegistry {
            scanned: true,
            ..Default::default()
        }
    }

    /// Takes a finished scan. Returns true when new plugins arrived.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(found) => {
                self.rx = None;
                self.accept(found);
                true
            }
            Err(_) => false,
        }
    }

    /// Every installed plugin, CLAP and VST3, sorted by name.
    pub fn plugins(&self) -> &[PluginInfo] {
        &self.list
    }

    pub fn is_scanning(&self) -> bool {
        !self.scanned
    }

    fn clap_info(&self, type_id: &str) -> Option<&ClapPluginInfo> {
        let id = type_id.strip_prefix("clap:")?;
        self.clap.iter().find(|p| p.id == id)
    }

    fn vst3_info(&self, type_id: &str) -> Option<&Vst3PluginInfo> {
        let cid = type_id.strip_prefix("vst3:")?;
        self.vst3.iter().find(|p| p.cid == cid)
    }

    fn is_known(&self, type_id: &str) -> bool {
        self.clap_info(type_id).is_some() || self.vst3_info(type_id).is_some()
    }

    /// Creates a plugin instance for `inst`, restoring its saved state.
    pub fn create(
        &mut self,
        inst: &DeviceInstance,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<Box<dyn Device>, String> {
        if self.failed.contains(&inst.id) {
            return Err(format!("{} failed to load", inst.type_id));
        }
        match self.instantiate(inst, sample_rate, max_block, false) {
            Ok((dev, handle)) => {
                if let Some(old) = self.handles.insert(inst.id, handle) {
                    old.close_editor();
                }
                Ok(dev)
            }
            Err(e) => {
                if self.is_known(&inst.type_id) {
                    self.failed.insert(inst.id);
                }
                Err(e)
            }
        }
    }

    /// A separate instance for offline rendering. The caller keeps the
    /// handle alive on the main thread for as long as the device runs.
    pub fn create_offline(
        &self,
        inst: &DeviceInstance,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<(Box<dyn Device>, PluginHandle), String> {
        self.instantiate(inst, sample_rate, max_block, true)
    }

    fn instantiate(
        &self,
        inst: &DeviceInstance,
        sample_rate: f32,
        max_block: usize,
        offline: bool,
    ) -> Result<(Box<dyn Device>, PluginHandle), String> {
        if let Some(info) = self.clap_info(&inst.type_id) {
            let mut dev =
                ClapDevice::load(info, sample_rate, max_block).map_err(|e| e.to_string())?;
            if let Some(bytes) = decode_state(&info.name, &inst.state)
                && let Err(e) = dev.load_state(&bytes)
            {
                log::warn!("{}: could not restore state: {e}", info.name);
            }
            if offline {
                dev.set_offline_render(true);
            }
            let handle = PluginHandle::Clap(Box::new(dev.handle()));
            return Ok((Box::new(dev), handle));
        }
        if let Some(info) = self.vst3_info(&inst.type_id) {
            let mut dev = Vst3Device::load(info, sample_rate, max_block)?;
            if let Some(bytes) = decode_state(&info.name, &inst.state)
                && let Err(e) = dev.load_state(&bytes)
            {
                log::warn!("{}: could not restore state: {e}", info.name);
            }
            if offline {
                dev.set_offline_render(true);
            }
            let handle = PluginHandle::Vst3(dev.handle());
            return Ok((Box::new(dev), handle));
        }
        Err(format!("{} is not installed", inst.type_id))
    }

    /// The plugin's own display text for a parameter value.
    pub fn param_text(&self, device: Id, id: ParamId, value: f32) -> Option<String> {
        self.handles.get(&device)?.param_text(id, value)
    }

    pub fn forget(&mut self, device: Id) {
        if let Some(h) = self.handles.remove(&device) {
            h.close_editor();
        }
    }

    pub fn handle(&self, device: Id) -> Option<&PluginHandle> {
        self.handles.get(&device)
    }

    /// Main-thread housekeeping every plugin asks for; call once a frame.
    pub fn idle(&self) -> Vec<Id> {
        self.handles
            .iter()
            .filter_map(|(&id, handle)| handle.idle().then_some(id))
            .collect()
    }

    /// Current state of every live plugin instance, base64-encoded.
    pub fn capture_states(&self) -> Vec<(Id, String)> {
        self.handles
            .iter()
            .filter_map(|(id, h)| {
                h.save_state()
                    .ok()
                    .map(|b| (*id, base64::engine::general_purpose::STANDARD.encode(b)))
            })
            .collect()
    }

    /// Brings stored parameter values of `inst` up to date with changes made
    /// in the plugin's own window. Such a change would otherwise be
    /// overwritten by the stale stored value when the project is reopened.
    /// Values the host changed last are left alone.
    pub fn sync_params(&self, inst: &mut DeviceInstance) {
        let Some(h) = self.handles.get(&inst.id) else {
            return;
        };
        for (key, value) in inst.params.iter_mut() {
            if let Some(live) = key
                .parse::<ParamId>()
                .ok()
                .and_then(|id| h.edited_in_window(id))
            {
                *value = live;
            }
        }
    }

    /// Closes every open plugin window.
    pub fn close_editors(&self) {
        for h in self.handles.values() {
            h.close_editor();
        }
    }
}
