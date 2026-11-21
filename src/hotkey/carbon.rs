//! Carbon 热键注册。接口来自 HIToolbox 的 CarbonEvents.h（64 位仍可用，按资料不需要辅助功能权限，待真机确认）。

use std::ffi::c_void;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use super::HotKey;

type OsStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventHandlerUpp = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OsStatus;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Default)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

const fn four_cc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const NO_ERR: OsStatus = 0;
const EVENT_NOT_HANDLED: OsStatus = -9874;
const EVENT_CLASS_KEYBOARD: u32 = four_cc(b"keyb");
const EVENT_HOT_KEY_PRESSED: u32 = 5;
const PARAM_DIRECT_OBJECT: u32 = four_cc(b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = four_cc(b"hkid");
/// 会话台的热键签名，用来认出是自己注册的。
const SIGNATURE: u32 = four_cc(b"htai");

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerUpp,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OsStatus;
    fn RemoveEventHandler(handler: EventHandlerRef) -> OsStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OsStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OsStatus;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OsStatus;
    fn RunApplicationEventLoop();
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

struct Callback {
    id: u32,
    cb: Box<dyn Fn() + Send>,
}

/// 一个已注册的热键；丢掉它就注销。含 Carbon 指针，不能跨线程。
pub struct Registration {
    hot_key: EventHotKeyRef,
    handler: EventHandlerRef,
    callback: *mut Callback,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: 三个指针都由 `register` 成功创建且只在这里释放一次；先注销热键和处理器，
        // Carbon 之后不会再用 user_data，再收回回调。
        unsafe {
            UnregisterEventHotKey(self.hot_key);
            RemoveEventHandler(self.handler);
            drop(Box::from_raw(self.callback));
        }
    }
}

extern "C" fn on_hot_key(
    _: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> OsStatus {
    let mut pressed = EventHotKeyId::default();
    // SAFETY: event 是 Carbon 传进来的有效事件；缓冲区大小与 EventHotKeyID 一致。
    let status = unsafe {
        GetEventParameter(
            event,
            PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            ptr::null_mut(),
            size_of::<EventHotKeyId>(),
            ptr::null_mut(),
            (&raw mut pressed).cast(),
        )
    };
    // SAFETY: user_data 是 `register` 里放进去的 Callback，Registration 活着它就有效。
    let callback = unsafe { &*user_data.cast::<Callback>() };
    if status != NO_ERR || pressed.signature != SIGNATURE || pressed.id != callback.id {
        return EVENT_NOT_HANDLED;
    }
    // 回调 panic 不能穿过 C 边界。
    let _ = catch_unwind(AssertUnwindSafe(|| (callback.cb)()));
    NO_ERR
}

/// 注册全局热键，按下时调用 `cb`。要在主线程调用，并在主线程跑 `run_event_loop` 才收得到。
/// 别的程序已占了同一组合时返回错误（系统不允许重复注册）。
pub fn register(key: HotKey, cb: impl Fn() + Send + 'static) -> io::Result<Registration> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let callback = Box::into_raw(Box::new(Callback {
        id,
        cb: Box::new(cb),
    }));
    let spec = EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_PRESSED,
    };
    let mut handler = ptr::null_mut();
    let mut hot_key = ptr::null_mut();
    // SAFETY: 参数都是有效指针；callback 在 Registration 释放前一直有效；失败路径收回已建的资源。
    unsafe {
        let target = GetApplicationEventTarget();
        let status =
            InstallEventHandler(target, on_hot_key, 1, &spec, callback.cast(), &mut handler);
        if status != NO_ERR {
            drop(Box::from_raw(callback));
            return Err(io::Error::other(format!(
                "装热键处理器失败（OSStatus {status}）"
            )));
        }
        let hot_key_id = EventHotKeyId {
            signature: SIGNATURE,
            id,
        };
        let status = RegisterEventHotKey(
            key.key_code,
            key.modifiers,
            hot_key_id,
            target,
            0,
            &mut hot_key,
        );
        if status != NO_ERR {
            RemoveEventHandler(handler);
            drop(Box::from_raw(callback));
            return Err(io::Error::other(format!(
                "注册热键失败（OSStatus {status}，可能已被别的程序占用）"
            )));
        }
    }
    Ok(Registration {
        hot_key,
        handler,
        callback,
    })
}

/// 在主线程跑 Carbon 事件循环，阻塞到进程结束。不在主线程时直接返回错误。
pub fn run_event_loop() -> io::Result<()> {
    // SAFETY: pthread_main_np 无参数、只读线程状态。
    if unsafe { libc::pthread_main_np() } != 1 {
        return Err(io::Error::other("热键事件循环必须在主线程跑"));
    }
    // SAFETY: 在主线程调用，符合 Carbon 要求。
    unsafe { RunApplicationEventLoop() };
    Ok(())
}
