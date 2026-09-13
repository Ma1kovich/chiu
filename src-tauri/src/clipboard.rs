#[cfg(target_os = "macos")]
pub(crate) fn write_text(text: &str) -> Result<(), ()> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let text = NSString::from_str(text);
    // AppKit owns this process-global string-type constant for the framework lifetime.
    let string_type = unsafe { NSPasteboardTypeString };
    pasteboard
        .setString_forType(&text, string_type)
        .then_some(())
        .ok_or(())
}

#[cfg(target_os = "windows")]
pub(crate) fn write_text(text: &str) -> Result<(), ()> {
    use std::{mem::size_of, ptr};
    use windows_sys::Win32::{
        Foundation::GlobalFree,
        System::{
            DataExchange::{EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
    };

    const CF_UNICODETEXT: u32 = 13;
    let encoded = text.encode_utf16().chain([0]).collect::<Vec<_>>();
    let bytes = encoded.len().checked_mul(size_of::<u16>()).ok_or(())?;

    // System ownership starts only after SetClipboardData accepts the movable allocation.
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            return Err(());
        }
        let _clipboard = ClipboardGuard;
        if EmptyClipboard() == 0 {
            return Err(());
        }
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() {
            return Err(());
        }
        let destination = GlobalLock(memory).cast::<u16>();
        if destination.is_null() {
            GlobalFree(memory);
            return Err(());
        }
        destination.copy_from_nonoverlapping(encoded.as_ptr(), encoded.len());
        GlobalUnlock(memory);
        if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
            GlobalFree(memory);
            return Err(());
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
struct ClipboardGuard;

#[cfg(target_os = "windows")]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // The guard exists only after the matching OpenClipboard call succeeds.
        unsafe {
            windows_sys::Win32::System::DataExchange::CloseClipboard();
        }
    }
}
