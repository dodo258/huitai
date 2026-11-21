//! 防休眠：有对话在跑时持有 IOPMAssertion（等价 `caffeinate -i`），全部停下就释放。

use std::ffi::{CString, c_char, c_void};

type CFStringRef = *const c_void;
type IOPMAssertionID = u32;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
const K_IO_RETURN_SUCCESS: i32 = 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut IOPMAssertionID,
    ) -> i32;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> i32;
}

/// 断言名，`pmset -g assertions` 里看得到（pmset 不显示非 ASCII 字符，所以用英文）。
pub const ASSERTION_NAME: &str = "huitai: hosted AI session running";

/// 持有期间阻止系统空闲休眠；drop 时释放。
pub struct PowerAssertion(IOPMAssertionID);

impl PowerAssertion {
    pub fn acquire() -> Option<Self> {
        let kind = cf_string("PreventUserIdleSystemSleep")?;
        let Some(name) = cf_string(ASSERTION_NAME) else {
            // SAFETY: kind 是刚创建、归我们所有的 CFString。
            unsafe { CFRelease(kind) };
            return None;
        };
        let mut id = 0;
        // SAFETY: 两个 CFString 有效，id 指向可写的局部变量；用完释放 CFString。
        let rc = unsafe {
            let rc = IOPMAssertionCreateWithName(kind, K_IOPM_ASSERTION_LEVEL_ON, name, &mut id);
            CFRelease(kind);
            CFRelease(name);
            rc
        };
        (rc == K_IO_RETURN_SUCCESS).then_some(Self(id))
    }
}

impl Drop for PowerAssertion {
    fn drop(&mut self) {
        // SAFETY: id 来自成功的 IOPMAssertionCreateWithName，只释放一次。
        unsafe {
            IOPMAssertionRelease(self.0);
        }
    }
}

fn cf_string(s: &str) -> Option<CFStringRef> {
    let c = CString::new(s).ok()?;
    // SAFETY: c 是以 NUL 结尾的有效 UTF-8；返回的 CFString 由调用方释放。
    let r = unsafe {
        CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    };
    (!r.is_null()).then_some(r)
}
