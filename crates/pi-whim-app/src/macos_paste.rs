#[cfg(target_os = "macos")]
use std::{
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{rc::Retained, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

/// A local AppKit monitor sees a Finder file paste before winit reduces it to
/// an empty text paste. It also captures raw clipboard images, which do not
/// produce an egui paste event when no text representation is available.
pub enum ClipboardAttachment {
    Paths(Vec<std::path::PathBuf>),
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
}

#[cfg(target_os = "macos")]
pub struct FinderPasteMonitor {
    composer_focused: Arc<AtomicBool>,
    queued_attachments: Arc<Mutex<Vec<ClipboardAttachment>>>,
    _monitor: Retained<AnyObject>,
}

#[cfg(target_os = "macos")]
impl FinderPasteMonitor {
    pub fn install() -> Option<Self> {
        let composer_focused = Arc::new(AtomicBool::new(false));
        let callback_focused = composer_focused.clone();
        let queued_attachments = Arc::new(Mutex::new(Vec::new()));
        let callback_attachments = queued_attachments.clone();
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
            if !callback_focused.load(Ordering::Relaxed) {
                event as *const NSEvent as *mut NSEvent
            } else {
                let attachment = arboard::Clipboard::new().ok().and_then(|mut clipboard| {
                    clipboard
                        .get()
                        .file_list()
                        .ok()
                        .filter(|paths| !paths.is_empty())
                        .map(ClipboardAttachment::Paths)
                        .or_else(|| {
                            clipboard
                                .get_image()
                                .ok()
                                .map(|image| ClipboardAttachment::Image {
                                    width: image.width,
                                    height: image.height,
                                    rgba: image.bytes.into_owned(),
                                })
                        })
                });
                if let Some(attachment) = attachment {
                    if let Ok(mut queued) = callback_attachments.lock() {
                        queued.push(attachment);
                    }
                    ptr::null_mut()
                } else {
                    event as *const NSEvent as *mut NSEvent
                }
            }
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &callback)
        }?;
        Some(Self {
            composer_focused,
            queued_attachments,
            _monitor: monitor,
        })
    }

    pub fn set_composer_focused(&self, focused: bool) {
        self.composer_focused.store(focused, Ordering::Relaxed);
    }

    pub fn drain_attachments(&self) -> Vec<ClipboardAttachment> {
        self.queued_attachments
            .lock()
            .map(|mut attachments| std::mem::take(&mut *attachments))
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

    pub fn set_composer_focused(&self, _focused: bool) {}

    pub fn drain_attachments(&self) -> Vec<ClipboardAttachment> {
        Vec::new()
    }
}
