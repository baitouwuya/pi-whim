#[cfg(target_os = "macos")]
use std::{
    ptr,
    sync::{Arc, Mutex},
};

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{rc::Retained, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

/// A local AppKit monitor sees a Finder file paste before winit reduces it to
/// an empty text paste. It only claims Cmd+V when a file list is present.
#[cfg(target_os = "macos")]
pub struct FinderPasteMonitor {
    queued_paths: Arc<Mutex<Vec<std::path::PathBuf>>>,
    _monitor: Retained<AnyObject>,
}

#[cfg(target_os = "macos")]
impl FinderPasteMonitor {
    pub fn install() -> Option<Self> {
        let queued_paths = Arc::new(Mutex::new(Vec::new()));
        let callback_paths = queued_paths.clone();
        let callback = RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| {
            let event = unsafe { event.as_ref() };
            let is_command_v = event
                .charactersIgnoringModifiers()
                .is_some_and(|characters| characters.to_string().eq_ignore_ascii_case("v"))
                && event
                    .modifierFlags()
                    .contains(NSEventModifierFlags::Command);
            if !is_command_v {
                return event as *const NSEvent as *mut NSEvent;
            }
            let paths = arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.get().file_list())
                .unwrap_or_default();
            if paths.is_empty() {
                event as *const NSEvent as *mut NSEvent
            } else {
                if let Ok(mut queued) = callback_paths.lock() {
                    queued.extend(paths);
                }
                ptr::null_mut()
            }
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &callback)
        }?;
        Some(Self {
            queued_paths,
            _monitor: monitor,
        })
    }

    pub fn drain_paths(&self) -> Vec<std::path::PathBuf> {
        self.queued_paths
            .lock()
            .map(|mut paths| std::mem::take(&mut *paths))
            .unwrap_or_default()
    }
}

#[cfg(not(target_os = "macos"))]
pub struct FinderPasteMonitor;

#[cfg(not(target_os = "macos"))]
impl FinderPasteMonitor {
    pub fn install() -> Option<Self> {
        None
    }

    pub fn drain_paths(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}
