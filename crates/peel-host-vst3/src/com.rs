//! ホスト側で用意する COM オブジェクト。
//!
//! プラグインはホストに、アプリの情報（[`HostApp`]）、メッセージ
//! （[`HostMessage`]）、状態を読み書きする入れ物（[`MemoryStream`]）、
//! つまみが動いたことの知らせ先（[`ComponentHandler`]）を求める。
//! 音の処理に渡すイベントとパラメータ変化の入れ物（[`EventList`]、
//! [`ParamChanges`]）は最初に確保し、音声スレッドでは確保し直さない。

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex, PoisonError};

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

/// VST3 の UTF-16 文字列を Rust の文字列にする。
pub(crate) fn from_wide(s: &[TChar]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// 文字列を VST3 の UTF-16 の欄に書く（はみ出す分は切る）。
pub(crate) fn to_wide(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (d, s) in dst.iter_mut().zip(src.encode_utf16()) {
        *d = s as TChar;
        n += 1;
    }
    let end = n.min(dst.len().saturating_sub(1));
    if let Some(last) = dst.get_mut(end) {
        *last = 0;
    }
}

/// 固定長の char8 欄を文字列にする。
pub(crate) fn from_c(s: &[c_char]) -> String {
    let bytes: Vec<u8> = s
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn same_guid(a: *const TUID, b: &[u8; 16]) -> bool {
    if a.is_null() {
        return false;
    }
    // SAFETY: TUID は 16 バイト。
    let a = unsafe { std::slice::from_raw_parts(a as *const u8, 16) };
    a == b
}

// ---- IHostApplication ----

pub(crate) struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        to_wide("Peachey Studio", unsafe { &mut *name });
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if obj.is_null() {
            return kInvalidArgument;
        }
        let made: Option<*mut c_void> =
            if same_guid(cid, &IMessage::IID) && same_guid(iid, &IMessage::IID) {
                ComWrapper::new(HostMessage::default())
                    .to_com_ptr::<IMessage>()
                    .map(|p| p.into_raw() as *mut c_void)
            } else if same_guid(cid, &IAttributeList::IID) && same_guid(iid, &IAttributeList::IID) {
                ComWrapper::new(AttributeList::default())
                    .to_com_ptr::<IAttributeList>()
                    .map(|p| p.into_raw() as *mut c_void)
            } else {
                None
            };
        match made {
            Some(p) => {
                unsafe { *obj = p };
                kResultOk
            }
            None => {
                unsafe { *obj = std::ptr::null_mut() };
                kNoInterface
            }
        }
    }
}

// ---- IMessage / IAttributeList ----

#[derive(Clone)]
enum Attr {
    Int(i64),
    Float(f64),
    Text(Vec<TChar>),
    Binary(Vec<u8>),
}

#[derive(Default)]
pub(crate) struct AttributeList {
    values: Mutex<HashMap<String, Attr>>,
}

impl AttributeList {
    fn key(id: AttrID) -> Option<String> {
        if id.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(id) }.to_string_lossy().into_owned())
    }

    fn get(&self, id: AttrID) -> Option<Attr> {
        let key = Self::key(id)?;
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned()
    }

    fn set(&self, id: AttrID, v: Attr) -> tresult {
        match Self::key(id) {
            Some(key) => {
                self.values
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key, v);
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.set(id, Attr::Int(value))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match self.get(id) {
            Some(Attr::Int(v)) if !value.is_null() => {
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.set(id, Attr::Float(value))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match self.get(id) {
            Some(Attr::Float(v)) if !value.is_null() => {
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut v = Vec::new();
        let mut i = 0;
        loop {
            let c = unsafe { *string.add(i) };
            v.push(c);
            if c == 0 {
                break;
            }
            i += 1;
        }
        self.set(id, Attr::Text(v))
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        match self.get(id) {
            Some(Attr::Text(v)) if !string.is_null() => {
                let cap = size_in_bytes as usize / std::mem::size_of::<TChar>();
                if cap == 0 {
                    return kResultFalse;
                }
                let n = v.len().min(cap);
                unsafe {
                    std::ptr::copy_nonoverlapping(v.as_ptr(), string, n);
                    *string.add(n - 1) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let bytes = if data.is_null() || size_in_bytes == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }
                .to_vec()
        };
        self.set(id, Attr::Binary(bytes))
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        let Some(key) = Self::key(id) else {
            return kInvalidArgument;
        };
        // 返すポインタはこのリストが生きている間だけ有効（SDK と同じ約束）。
        let map = self.values.lock().unwrap_or_else(PoisonError::into_inner);
        match map.get(&key) {
            Some(Attr::Binary(v)) => {
                unsafe {
                    *data = v.as_ptr() as *const c_void;
                    *size_in_bytes = v.len() as uint32;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

pub(crate) struct HostMessage {
    id: Mutex<Option<std::ffi::CString>>,
    attributes: ComWrapper<AttributeList>,
}

impl Default for HostMessage {
    fn default() -> Self {
        HostMessage {
            id: Mutex::new(None),
            attributes: ComWrapper::new(AttributeList::default()),
        }
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(std::ptr::null(), |s| s.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let v = if id.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(id) }.to_owned())
        };
        *self.id.lock().unwrap_or_else(PoisonError::into_inner) = v;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // 借用として返す（参照カウントは増やさない）。
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

// ---- IBStream ----

/// 状態を読み書きするメモリ上の入れ物。
#[derive(Default)]
pub(crate) struct MemoryStream {
    inner: Mutex<(Vec<u8>, usize)>,
}

impl MemoryStream {
    pub(crate) fn with_data(data: &[u8]) -> MemoryStream {
        MemoryStream {
            inner: Mutex::new((data.to_vec(), 0)),
        }
    }

    pub(crate) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.inner.lock().unwrap_or_else(PoisonError::into_inner).0)
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (data, pos) = &mut *g;
        let want = num_bytes.max(0) as usize;
        let n = want.min(data.len().saturating_sub(*pos));
        if n > 0 && !buffer.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer as *mut u8, n) };
        }
        *pos += n;
        if !num_read.is_null() {
            unsafe { *num_read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_written: *mut int32,
    ) -> tresult {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (data, pos) = &mut *g;
        let n = num_bytes.max(0) as usize;
        if n > 0 && !buffer.is_null() {
            let src = unsafe { std::slice::from_raw_parts(buffer as *const u8, n) };
            if data.len() < *pos + n {
                data.resize(*pos + n, 0);
            }
            data[*pos..*pos + n].copy_from_slice(src);
        }
        *pos += n;
        if !num_written.is_null() {
            unsafe { *num_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (data, cur) = &mut *g;
        let base = match mode {
            0 => 0i64,              // kIBSeekSet
            1 => *cur as i64,       // kIBSeekCur
            2 => data.len() as i64, // kIBSeekEnd
            _ => return kInvalidArgument,
        };
        let next = base + pos;
        if next < 0 {
            return kInvalidArgument;
        }
        *cur = next as usize;
        if !result.is_null() {
            unsafe { *result = next };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = g.1 as int64 };
        kResultOk
    }
}

// ---- IComponentHandler ----

/// プラグインの画面でつまみが動いた、作り直してほしい、などの知らせ。
/// どれもメインスレッドで届く。
pub(crate) trait EditSink: Send + Sync {
    fn perform_edit(&self, id: ParamID, normalized: f64);
    fn restart(&self, flags: i32);
}

pub(crate) struct ComponentHandler {
    pub(crate) sink: Arc<dyn EditSink>,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.sink.perform_edit(id, value);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.sink.restart(flags);
        kResultOk
    }
}

// ---- IEventList ----

/// 1 ブロック分のノートイベント。音声スレッドだけが触る。
pub(crate) struct EventList {
    events: UnsafeCell<Vec<Event>>,
}

// SAFETY: 中身に触るのは、プラグインの process 呼び出しの間の音声スレッド
// だけ。その間ホストの側は触らない。
unsafe impl Send for EventList {}
unsafe impl Sync for EventList {}

pub(crate) const MAX_EVENTS: usize = 1024;

impl EventList {
    pub(crate) fn new() -> EventList {
        EventList {
            events: UnsafeCell::new(Vec::with_capacity(MAX_EVENTS)),
        }
    }

    /// # Safety
    /// process の外の音声スレッドからだけ呼ぶ。
    pub(crate) unsafe fn clear(&self) {
        unsafe { (*self.events.get()).clear() };
    }

    /// # Safety
    /// process の外の音声スレッドからだけ呼ぶ。容量を超えた分は捨てる。
    pub(crate) unsafe fn push(&self, e: Event) {
        let v = unsafe { &mut *self.events.get() };
        if v.len() < v.capacity() {
            v.push(e);
        }
    }
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        unsafe { (*self.events.get()).len() as int32 }
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        let v = unsafe { &*self.events.get() };
        match v.get(index as usize) {
            Some(ev) if !e.is_null() => {
                unsafe { std::ptr::copy_nonoverlapping(ev, e, 1) };
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, _e: *mut Event) -> tresult {
        // 出力イベントは受け取らない。
        kResultFalse
    }
}

// ---- IParameterChanges / IParamValueQueue ----

const MAX_POINTS: usize = 64;

pub(crate) struct ParamQueue {
    data: UnsafeCell<(ParamID, Vec<(i32, f64)>)>,
}

unsafe impl Send for ParamQueue {}
unsafe impl Sync for ParamQueue {}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        unsafe { (*self.data.get()).0 }
    }

    unsafe fn getPointCount(&self) -> int32 {
        unsafe { (*self.data.get()).1.len() as int32 }
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        let d = unsafe { &*self.data.get() };
        match d.1.get(index as usize) {
            Some(&(o, v)) if !offset.is_null() && !value.is_null() => {
                unsafe {
                    *offset = o;
                    *value = v;
                }
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addPoint(&self, _offset: int32, _value: ParamValue, _index: *mut int32) -> tresult {
        kResultFalse
    }
}

/// 1 ブロック分のパラメータ変化。最初に必要な数のキューを作っておく。
pub(crate) struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    used: UnsafeCell<usize>,
}

unsafe impl Send for ParamChanges {}
unsafe impl Sync for ParamChanges {}

impl ParamChanges {
    pub(crate) fn new(capacity: usize) -> ParamChanges {
        ParamChanges {
            queues: (0..capacity.max(8))
                .map(|_| {
                    ComWrapper::new(ParamQueue {
                        data: UnsafeCell::new((0, Vec::with_capacity(MAX_POINTS))),
                    })
                })
                .collect(),
            used: UnsafeCell::new(0),
        }
    }

    /// # Safety
    /// process の外の音声スレッドからだけ呼ぶ。
    pub(crate) unsafe fn clear(&self) {
        unsafe { *self.used.get() = 0 };
    }

    /// # Safety
    /// process の外の音声スレッドからだけ呼ぶ。入りきらない分は捨てる。
    pub(crate) unsafe fn add(&self, id: ParamID, offset: i32, value: f64) {
        let used = unsafe { &mut *self.used.get() };
        let found = self.queues[..*used]
            .iter()
            .find(|q| unsafe { (*q.data.get()).0 } == id);
        let q = match found {
            Some(q) => q,
            None => {
                if *used >= self.queues.len() {
                    return;
                }
                let q = &self.queues[*used];
                *used += 1;
                let d = unsafe { &mut *q.data.get() };
                d.0 = id;
                d.1.clear();
                q
            }
        };
        let d = unsafe { &mut *q.data.get() };
        // 同じ位置なら後の値で上書きし、位置は昇順に保つ。
        if let Some(last) = d.1.last_mut()
            && last.0 >= offset
        {
            last.1 = value;
            return;
        }
        if d.1.len() < d.1.capacity() {
            d.1.push((offset, value));
        } else if let Some(last) = d.1.last_mut() {
            last.1 = value;
        }
    }
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        unsafe { *self.used.get() as int32 }
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        let used = unsafe { *self.used.get() };
        if index < 0 || index as usize >= used {
            return std::ptr::null_mut();
        }
        self.queues[index as usize]
            .as_com_ref::<IParamValueQueue>()
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }

    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

// ---- IPlugFrame ----

/// プラグインの画面が大きさを変えたいときの知らせ先。
pub(crate) trait ResizeSink: Send + Sync {
    fn resize(&self, width: i32, height: i32) -> bool;
}

pub(crate) struct PlugFrame {
    pub(crate) sink: Arc<dyn ResizeSink>,
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() {
            return kInvalidArgument;
        }
        let r = unsafe { &mut *new_size };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if !self.sink.resize(w, h) {
            return kResultFalse;
        }
        if !view.is_null() {
            // SAFETY: プラグインが渡した生きているビュー。
            unsafe { ((*(*view).vtbl).onSize)(view, new_size) };
        }
        kResultOk
    }
}
