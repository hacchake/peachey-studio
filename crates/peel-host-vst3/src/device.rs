//! VST3 プラグインを [`peel_dsp::Device`] として動かす。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use peel_dsp::{Device, DeviceDescriptor, DeviceEvent, EventKind, ParamId, ProcessContext};
use vst3::ComWrapper;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;

use crate::com::{EventList, ParamChanges};
use crate::handle::Vst3Handle;
use crate::info::Vst3PluginInfo;
use crate::plugin::Plugin;

/// 同時に覚えておく発音の数（全消音のときにノートオフを送るため）。
const MAX_VOICES: usize = 256;

#[derive(Clone, Copy)]
struct Voice {
    id: u32,
    key: i16,
}

struct Bus {
    channels: Vec<Vec<f32>>,
    ptrs: Vec<*mut f32>,
}

impl Bus {
    fn new(channels: usize, max_block: usize) -> Bus {
        let mut channels: Vec<Vec<f32>> =
            (0..channels).map(|_| vec![0.0; max_block.max(1)]).collect();
        let ptrs = channels.iter_mut().map(|c| c.as_mut_ptr()).collect();
        Bus { channels, ptrs }
    }
}

/// 音声スレッド用の、最初に確保しておく入れ物。
struct Realtime {
    events: ComWrapper<EventList>,
    changes: ComWrapper<ParamChanges>,
    /// ホストが変えたパラメータ（次のブロックの頭で送る）。
    pending: Vec<(u32, f64)>,
    inputs: Vec<Bus>,
    outputs: Vec<Bus>,
    in_buffers: Vec<AudioBusBuffers>,
    out_buffers: Vec<AudioBusBuffers>,
    context: vst3::Steinberg::Vst::ProcessContext,
    voices: Vec<Voice>,
    release_all: bool,
    bend_range: f32,
    position: i64,
    has_event_input: bool,
}

/// VST3 プラグイン 1 つ。エンジンには普通のデバイスとして渡す。
pub struct Vst3Device {
    plugin: Arc<Plugin>,
    gui_rx: rtrb::Consumer<(u32, f64)>,
    rt: Box<Realtime>,
    prepared: Option<(f32, usize)>,
    active: bool,
}

// SAFETY: 生ポインタは自分の持つバッファを指すだけ。プラグインへの呼び出しは
// VST3 の約束どおりのスレッドで行う（Plugin の説明を参照）。
unsafe impl Send for Vst3Device {}

fn ok(r: tresult) -> bool {
    r == kResultOk || r == kResultTrue
}

impl Vst3Device {
    /// プラグインを作って準備する。メインスレッドで呼ぶ。
    pub fn load(
        info: &Vst3PluginInfo,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<Vst3Device, String> {
        let (plugin, gui_rx) = Plugin::load(info)?;
        let capacity = plugin.shared.specs.len() + 64;
        let mut dev = Vst3Device {
            plugin,
            gui_rx,
            rt: Box::new(Realtime {
                events: ComWrapper::new(EventList::new()),
                changes: ComWrapper::new(ParamChanges::new(capacity.min(4096))),
                pending: Vec::with_capacity(capacity),
                inputs: Vec::new(),
                outputs: Vec::new(),
                in_buffers: Vec::new(),
                out_buffers: Vec::new(),
                context: unsafe { std::mem::zeroed() },
                voices: Vec::with_capacity(MAX_VOICES),
                release_all: false,
                bend_range: 2.0,
                position: 0,
                has_event_input: false,
            }),
            prepared: None,
            active: false,
        };
        dev.prepare(sample_rate, max_block);
        if !dev.active {
            return Err(format!(
                "{} を {sample_rate} Hz で動かせませんでした",
                info.name
            ));
        }
        Ok(dev)
    }

    /// メインスレッドで使う操作口（状態の保存、画面を開く、など）。
    pub fn handle(&self) -> Vst3Handle {
        Vst3Handle::new(self.plugin.clone())
    }

    /// 保存しておいた状態を戻す（エンジンに渡す前に）。
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.plugin.load_state(data)
    }

    /// 書き出し用（リアルタイムでなくてよい処理）として動かす。
    pub fn set_offline_render(&mut self, offline: bool) {
        self.plugin.shared.offline.store(offline, Ordering::Relaxed);
        if let Some((rate, block)) = self.prepared {
            self.prepared = None;
            self.prepare(rate, block);
        }
    }

    fn deactivate(&mut self) {
        if self.active {
            unsafe {
                self.plugin.processor.setProcessing(0);
                self.plugin.component.setActive(0);
            }
            self.active = false;
        }
    }

    fn queue_param(&mut self, id: u32, normalized: f64) {
        if self.rt.pending.len() < self.rt.pending.capacity() {
            self.rt.pending.push((id, normalized));
        }
    }

    fn set_normalized(&mut self, id: ParamId, plain: f32) -> Option<f64> {
        let shared = &self.plugin.shared;
        let &i = shared.index_of.get(&id)?;
        let n = shared.specs[i].normalized(plain);
        shared.set(i, n);
        shared.host_dirty[i].store(true, Ordering::Relaxed);
        shared.gui_edited[i].store(false, Ordering::Relaxed);
        Some(n)
    }

    unsafe fn note_off_event(offset: i32, key: i16, id: u32) -> Event {
        let mut e: Event = unsafe { std::mem::zeroed() };
        e.sampleOffset = offset;
        e.flags = Event_::EventFlags_::kIsLive as u16;
        e.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
        e.__field0.noteOff = NoteOffEvent {
            channel: 0,
            pitch: key,
            velocity: 0.0,
            noteId: (id & 0x7FFF_FFFF) as i32,
            tuning: 0.0,
        };
        e
    }

    fn release_voices(&mut self, offset: i32) {
        let rt = &mut *self.rt;
        for v in rt.voices.drain(..) {
            unsafe { rt.events.push(Self::note_off_event(offset, v.key, v.id)) };
        }
    }
}

impl Drop for Vst3Device {
    fn drop(&mut self) {
        self.deactivate();
    }
}

fn channel_count(arr: SpeakerArrangement) -> usize {
    arr.count_ones() as usize
}

impl Device for Vst3Device {
    fn descriptor(&self) -> &'static DeviceDescriptor {
        self.plugin.descriptor
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize) {
        if self.prepared == Some((sample_rate, max_block)) && self.active {
            return;
        }
        self.deactivate();
        let p = self.plugin.clone();
        let audio = MediaTypes_::kAudio as MediaType;
        let event = MediaTypes_::kEvent as MediaType;
        let input = BusDirections_::kInput as BusDirection;
        let output = BusDirections_::kOutput as BusDirection;
        unsafe {
            let n_in = p.component.getBusCount(audio, input).max(0);
            let n_out = p.component.getBusCount(audio, output).max(0);
            let n_ev = p.component.getBusCount(event, input).max(0);
            let current = |dir: BusDirection, i: i32| {
                let mut arr: SpeakerArrangement = 0;
                if ok(p.processor.getBusArrangement(dir, i, &mut arr)) {
                    arr
                } else {
                    SpeakerArr::kStereo
                }
            };
            // 主バスはステレオにしてほしいと頼む。断られたら相手の形に合わせる。
            let mut ins: Vec<SpeakerArrangement> = (0..n_in)
                .map(|i| {
                    if i == 0 {
                        SpeakerArr::kStereo
                    } else {
                        current(input, i)
                    }
                })
                .collect();
            let mut outs: Vec<SpeakerArrangement> = (0..n_out)
                .map(|i| {
                    if i == 0 {
                        SpeakerArr::kStereo
                    } else {
                        current(output, i)
                    }
                })
                .collect();
            p.processor
                .setBusArrangements(ins.as_mut_ptr(), n_in, outs.as_mut_ptr(), n_out);
            let ins: Vec<usize> = (0..n_in)
                .map(|i| channel_count(current(input, i)))
                .collect();
            let outs: Vec<usize> = (0..n_out)
                .map(|i| channel_count(current(output, i)))
                .collect();
            for i in 0..n_in {
                p.component.activateBus(audio, input, i, (i == 0) as TBool);
            }
            for i in 0..n_out {
                p.component.activateBus(audio, output, i, (i == 0) as TBool);
            }
            for i in 0..n_ev {
                p.component.activateBus(event, input, i, (i == 0) as TBool);
            }
            let offline = p.shared.offline.load(Ordering::Relaxed);
            let mut setup = ProcessSetup {
                processMode: if offline {
                    ProcessModes_::kOffline
                } else {
                    ProcessModes_::kRealtime
                },
                symbolicSampleSize: SymbolicSampleSizes_::kSample32,
                maxSamplesPerBlock: max_block as i32,
                sampleRate: sample_rate as f64,
            };
            if !ok(p.processor.setupProcessing(&mut setup)) {
                log::warn!("{}: setupProcessing を断られました", p.info.name);
            }
            if !ok(p.component.setActive(1)) {
                log::warn!("{}: 有効にできませんでした", p.info.name);
                return;
            }
            p.processor.setProcessing(1);
            p.shared
                .latency
                .store(p.processor.getLatencySamples(), Ordering::Relaxed);

            let rt = &mut *self.rt;
            rt.inputs = ins.iter().map(|&c| Bus::new(c, max_block)).collect();
            rt.outputs = outs.iter().map(|&c| Bus::new(c, max_block)).collect();
            let make = |b: &mut Bus| {
                let mut a: AudioBusBuffers = std::mem::zeroed();
                a.numChannels = b.ptrs.len() as i32;
                a.__field0.channelBuffers32 = b.ptrs.as_mut_ptr();
                a
            };
            rt.in_buffers = rt.inputs.iter_mut().map(make).collect();
            rt.out_buffers = rt.outputs.iter_mut().map(make).collect();
            rt.has_event_input = n_ev > 0;
            rt.context.sampleRate = sample_rate as f64;
        }
        self.prepared = Some((sample_rate, max_block));
        self.active = true;
    }

    fn reset(&mut self) {
        self.rt.release_all = true;
    }

    fn set_param(&mut self, id: ParamId, value: f32) {
        if let Some(n) = self.set_normalized(id, value) {
            self.queue_param(id, n);
        }
    }

    fn param(&self, id: ParamId) -> f32 {
        let shared = &self.plugin.shared;
        match shared.index_of.get(&id) {
            Some(&i) => shared.specs[i].plain(shared.get(i)),
            None => 0.0,
        }
    }

    fn process(
        &mut self,
        ctx: &ProcessContext,
        events: &[DeviceEvent],
        left: &mut [f32],
        right: &mut [f32],
    ) {
        let frames = left.len().min(right.len());
        if !self.active || frames == 0 {
            left.fill(0.0);
            right.fill(0.0);
            return;
        }
        unsafe {
            self.rt.events.clear();
            self.rt.changes.clear();
        }
        if std::mem::take(&mut self.rt.release_all) {
            self.release_voices(0);
        }
        // プラグインの画面で動いた値と、ホストが変えた値を頭で送る。
        while let Ok((id, v)) = self.gui_rx.pop() {
            unsafe { self.rt.changes.add(id, 0, v) };
        }
        // 確保し直さないよう、その場で送ってから空にする。
        for &(id, v) in &self.rt.pending {
            unsafe { self.rt.changes.add(id, 0, v) };
        }
        self.rt.pending.clear();

        for e in events {
            let offset = e.offset.min(frames as u32 - 1) as i32;
            match e.kind {
                EventKind::NoteOn {
                    key,
                    velocity,
                    voice,
                } => {
                    let mut ev: Event = unsafe { std::mem::zeroed() };
                    ev.sampleOffset = offset;
                    ev.flags = Event_::EventFlags_::kIsLive as u16;
                    ev.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
                    ev.__field0.noteOn = NoteOnEvent {
                        channel: 0,
                        pitch: key as i16,
                        tuning: 0.0,
                        velocity,
                        length: 0,
                        noteId: (voice & 0x7FFF_FFFF) as i32,
                    };
                    unsafe { self.rt.events.push(ev) };
                    if self.rt.voices.len() < MAX_VOICES {
                        self.rt.voices.push(Voice {
                            id: voice,
                            key: key as i16,
                        });
                    }
                }
                EventKind::NoteOff {
                    key,
                    velocity,
                    voice,
                } => {
                    let mut ev = unsafe { Self::note_off_event(offset, key as i16, voice) };
                    ev.__field0.noteOff.velocity = velocity;
                    unsafe { self.rt.events.push(ev) };
                    if let Some(i) = self.rt.voices.iter().position(|v| v.id == voice) {
                        self.rt.voices.swap_remove(i);
                    }
                }
                EventKind::PolyPressure { key, value } => {
                    let mut ev: Event = unsafe { std::mem::zeroed() };
                    ev.sampleOffset = offset;
                    ev.flags = Event_::EventFlags_::kIsLive as u16;
                    ev.r#type = Event_::EventTypes_::kPolyPressureEvent as u16;
                    ev.__field0.polyPressure = PolyPressureEvent {
                        channel: 0,
                        pitch: key as i16,
                        pressure: value,
                        noteId: -1,
                    };
                    unsafe { self.rt.events.push(ev) };
                }
                EventKind::Param { id, value } => {
                    if let Some(n) = self.set_normalized(id, value) {
                        unsafe { self.rt.changes.add(id, offset, n) };
                    }
                }
                EventKind::PitchBend { semitones } => {
                    if let Some(id) = self.plugin.pitch_bend {
                        let range = self.rt.bend_range.max(0.01);
                        let n = (0.5 + semitones as f64 / (2.0 * range as f64)).clamp(0.0, 1.0);
                        unsafe { self.rt.changes.add(id, offset, n) };
                    }
                }
                EventKind::PitchBendRange { semitones } => self.rt.bend_range = semitones,
                EventKind::ChannelPressure { value } => {
                    if let Some(id) = self.plugin.aftertouch {
                        unsafe {
                            self.rt
                                .changes
                                .add(id, offset, value.clamp(0.0, 1.0) as f64)
                        };
                    }
                }
                EventKind::AllNotesOff | EventKind::AllSoundOff => self.release_voices(offset),
                _ => {}
            }
        }

        let rt = &mut *self.rt;
        // 主入力にホストの左右を入れる（エフェクトのとき）。
        if let Some(bus) = rt.inputs.first_mut() {
            match bus.channels.len() {
                0 => {}
                1 => {
                    for (o, (l, r)) in bus.channels[0][..frames]
                        .iter_mut()
                        .zip(left.iter().zip(right.iter()))
                    {
                        *o = 0.5 * (l + r);
                    }
                }
                _ => {
                    bus.channels[0][..frames].copy_from_slice(&left[..frames]);
                    bus.channels[1][..frames].copy_from_slice(&right[..frames]);
                }
            }
        }
        for bus in &mut rt.outputs {
            for c in &mut bus.channels {
                c[..frames].fill(0.0);
            }
        }
        for b in rt.in_buffers.iter_mut().chain(rt.out_buffers.iter_mut()) {
            b.silenceFlags = 0;
        }

        use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_::*;
        rt.context.state = (kTempoValid | kProjectTimeMusicValid) as u32
            | if ctx.playing { kPlaying as u32 } else { 0 };
        rt.context.tempo = ctx.tempo_bpm;
        rt.context.projectTimeMusic = ctx.beat_position;
        rt.context.projectTimeSamples = rt.position;
        rt.context.sampleRate = ctx.sample_rate as f64;

        let offline = self.plugin.shared.offline.load(Ordering::Relaxed);
        let mut data = vst3::Steinberg::Vst::ProcessData {
            processMode: if offline {
                ProcessModes_::kOffline
            } else {
                ProcessModes_::kRealtime
            },
            symbolicSampleSize: SymbolicSampleSizes_::kSample32,
            numSamples: frames as i32,
            numInputs: rt.in_buffers.len() as i32,
            numOutputs: rt.out_buffers.len() as i32,
            inputs: if rt.in_buffers.is_empty() {
                std::ptr::null_mut()
            } else {
                rt.in_buffers.as_mut_ptr()
            },
            outputs: if rt.out_buffers.is_empty() {
                std::ptr::null_mut()
            } else {
                rt.out_buffers.as_mut_ptr()
            },
            inputParameterChanges: rt
                .changes
                .as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: if rt.has_event_input {
                rt.events
                    .as_com_ref::<IEventList>()
                    .map_or(std::ptr::null_mut(), |r| r.as_ptr())
            } else {
                std::ptr::null_mut()
            },
            outputEvents: std::ptr::null_mut(),
            processContext: &mut rt.context,
        };
        unsafe { self.plugin.processor.process(&mut data) };

        match rt.outputs.first() {
            Some(bus) if bus.channels.len() >= 2 => {
                left[..frames].copy_from_slice(&bus.channels[0][..frames]);
                right[..frames].copy_from_slice(&bus.channels[1][..frames]);
            }
            Some(bus) if bus.channels.len() == 1 => {
                left[..frames].copy_from_slice(&bus.channels[0][..frames]);
                right[..frames].copy_from_slice(&bus.channels[0][..frames]);
            }
            _ => {
                left.fill(0.0);
                right.fill(0.0);
            }
        }
        rt.position += frames as i64;
    }

    fn latency(&self) -> u32 {
        self.plugin.shared.latency.load(Ordering::Relaxed)
    }

    fn tail_seconds(&self) -> Option<f32> {
        let tail = unsafe { self.plugin.processor.getTailSamples() };
        if tail == kInfiniteTail {
            return None;
        }
        let rate = self.prepared.map_or(48_000.0, |(r, _)| r);
        Some(tail as f32 / rate.max(1.0))
    }
}
