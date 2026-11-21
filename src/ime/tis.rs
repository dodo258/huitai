//! Carbon Text Input Sources（HIToolbox 的 TextInputSources.h）。TIS 只能在主线程调。

use std::ffi::{CStr, c_char, c_void};
use std::io;

use super::{InputSource, InputSources};

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type TISInputSourceRef = *const c_void;
type OsStatus = i32;

const UTF8: u32 = 0x0800_0100;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyInputSourceIsASCIICapable: CFStringRef;
    static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
    static kTISPropertyInputSourceType: CFStringRef;
    static kTISTypeKeyboardLayout: CFStringRef;

    fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    fn TISCreateInputSourceList(properties: CFTypeRef, include_all_installed: u8) -> CFArrayRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> CFTypeRef;
    fn TISSelectInputSource(source: TISInputSourceRef) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
}

/// 系统的输入源（真切换）。
#[derive(Debug, Default, Clone, Copy)]
pub struct Tis;

/// 一个 TIS 对象的属性（不持有引用，只在对象活着时用）。
struct Props(TISInputSourceRef);

impl Props {
    fn get(&self, key: CFStringRef) -> CFTypeRef {
        // SAFETY: self.0 是活着的 TISInputSourceRef，key 是 Carbon 导出的常量；返回值不需要释放。
        unsafe { TISGetInputSourceProperty(self.0, key) }
    }

    fn id(&self) -> Option<String> {
        // SAFETY: 读 Carbon 导出的常量。
        let v = self.get(unsafe { kTISPropertyInputSourceID });
        if v.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 256];
        // SAFETY: v 是 CFString，buf 够长且以 0 结尾（失败时返回 0，不读 buf）。
        let ok = unsafe { CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as isize, UTF8) };
        if ok == 0 {
            return None;
        }
        // SAFETY: CFStringGetCString 成功时写了以 0 结尾的字符串。
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) };
        Some(s.to_string_lossy().into_owned())
    }

    fn flag(&self, key: CFStringRef) -> bool {
        let v = self.get(key);
        // SAFETY: 比较两个 CF 对象；kCFBooleanTrue 是常量。
        !v.is_null() && unsafe { CFEqual(v, kCFBooleanTrue) } != 0
    }

    fn ascii(&self) -> bool {
        // SAFETY: 读 Carbon 导出的常量。
        self.flag(unsafe { kTISPropertyInputSourceIsASCIICapable })
    }

    fn keyboard_layout(&self) -> bool {
        // SAFETY: 读 Carbon 导出的常量。
        let v = self.get(unsafe { kTISPropertyInputSourceType });
        // SAFETY: 两个都是 CFString。
        !v.is_null() && unsafe { CFEqual(v, kTISTypeKeyboardLayout) } != 0
    }

    fn selectable(&self) -> bool {
        // SAFETY: 读 Carbon 导出的常量。
        self.flag(unsafe { kTISPropertyInputSourceIsSelectCapable })
    }
}

/// 用户启用的全部输入源（含输入法的各个输入模式），对每个调 `f`，`f` 返回 `Some` 就停。
fn each_enabled<T>(mut f: impl FnMut(&Props) -> Option<T>) -> Option<T> {
    // SAFETY: 不传过滤条件，只列启用的；返回的数组归我们释放。
    let list = unsafe { TISCreateInputSourceList(std::ptr::null(), 0) };
    if list.is_null() {
        return None;
    }
    let mut out = None;
    // SAFETY: list 是有效的 CFArray，下标在范围内；元素由数组持有，数组释放前有效。
    let n = unsafe { CFArrayGetCount(list) };
    for i in 0..n {
        let p = Props(unsafe { CFArrayGetValueAtIndex(list, i) });
        out = f(&p);
        if out.is_some() {
            break;
        }
    }
    // SAFETY: list 是 Create 出来的，释放一次。
    unsafe { CFRelease(list) };
    out
}

impl InputSources for Tis {
    fn current(&self) -> Option<InputSource> {
        // SAFETY: 返回 +1 引用，用完释放。
        let src = unsafe { TISCopyCurrentKeyboardInputSource() };
        if src.is_null() {
            return None;
        }
        let p = Props(src);
        let out = p.id().map(|id| InputSource {
            id,
            ascii: p.ascii(),
        });
        // SAFETY: Copy 出来的，释放一次。
        unsafe { CFRelease(src) };
        out
    }

    fn ascii_layouts(&self) -> Vec<String> {
        let mut out = Vec::new();
        each_enabled::<()>(|p| {
            if p.keyboard_layout()
                && p.ascii()
                && p.selectable()
                && let Some(id) = p.id()
            {
                out.push(id);
            }
            None
        });
        out
    }

    fn select(&self, id: &str) -> io::Result<()> {
        let status = each_enabled(|p| {
            // SAFETY: p.0 在数组释放前有效。
            (p.id().as_deref() == Some(id)).then(|| unsafe { TISSelectInputSource(p.0) })
        });
        match status {
            Some(0) => Ok(()),
            Some(s) => Err(io::Error::other(format!(
                "切换输入源 {id} 失败：OSStatus {s}"
            ))),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("没有启用输入源 {id}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只读：看本机当前输入源和启用的英文布局（不切换）。`cargo test --lib ime::tis -- --ignored --nocapture`
    #[test]
    #[ignore = "读本机输入源，手动跑"]
    fn reads_current_source() {
        println!("当前：{:?}", Tis.current());
        println!("英文布局：{:?}", Tis.ascii_layouts());
    }
}
