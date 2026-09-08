use std::sync::Mutex;

use crate::settings::TaskbarButton;
use crate::theme::Icon;

pub const SLOTS: usize = TaskbarButton::ALL.len();

static CLICKS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static WAKER: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

const THBN_CLICKED: usize = 0x1800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskbarState {
    pub playing: bool,
    pub saved: bool,
    pub repeat_one: bool,
    pub has_track: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub icon: Icon,
    pub tip: &'static str,
    pub enabled: bool,
}

pub fn set_waker(wake: impl Fn() + Send + Sync + 'static) {
    if let Ok(mut waker) = WAKER.lock() {
        *waker = Some(Box::new(wake));
    }
}

fn wake() {
    if let Ok(waker) = WAKER.lock()
        && let Some(wake) = waker.as_ref()
    {
        wake();
    }
}

pub fn push_click(slot: usize) {
    if let Ok(mut clicks) = CLICKS.lock() {
        clicks.push(slot);
    }
    wake();
}

pub fn drain_clicks() -> Vec<usize> {
    CLICKS
        .lock()
        .map(|mut clicks| std::mem::take(&mut *clicks))
        .unwrap_or_default()
}

pub fn reorder(buttons: &mut Vec<TaskbarButton>, from: usize, to: usize) {
    if from >= buttons.len() || to > buttons.len() {
        return;
    }
    let button = buttons.remove(from);
    let at = if to > from { to - 1 } else { to };
    buttons.insert(at.min(buttons.len()), button);
}

pub fn clicked_slot(wparam: usize) -> Option<usize> {
    (((wparam >> 16) & 0xFFFF) == THBN_CLICKED)
        .then_some(wparam & 0xFFFF)
        .filter(|slot| *slot < SLOTS)
}

pub fn appearance(button: TaskbarButton, state: TaskbarState) -> Appearance {
    match button {
        TaskbarButton::Like => Appearance {
            icon: if state.saved {
                Icon::HeartFilled
            } else {
                Icon::Heart
            },
            tip: if state.saved {
                "Remove from Liked Songs"
            } else {
                "Add to Liked Songs"
            },
            enabled: state.has_track,
        },
        TaskbarButton::Previous => Appearance {
            icon: Icon::SkipBackFilled,
            tip: "Previous",
            enabled: true,
        },
        TaskbarButton::PlayPause => Appearance {
            icon: if state.playing {
                Icon::PauseFilled
            } else {
                Icon::PlayFilled
            },
            tip: if state.playing { "Pause" } else { "Play" },
            enabled: true,
        },
        TaskbarButton::Next => Appearance {
            icon: Icon::SkipForwardFilled,
            tip: "Next",
            enabled: true,
        },
        TaskbarButton::RepeatOne => Appearance {
            icon: if state.repeat_one {
                Icon::Repeat1
            } else {
                Icon::Repeat
            },
            tip: if state.repeat_one {
                "Stop repeating this song"
            } else {
                "Repeat this song"
            },
            enabled: state.has_track,
        },
    }
}

#[cfg(windows)]
pub use platform::attach;

#[cfg(windows)]
mod platform {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use egui::load::{ImagePoll, SizeHint};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        DefSubclassProc, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass, THB_FLAGS,
        THB_ICON, THB_TOOLTIP, THBF_DISABLED, THBF_ENABLED, THBF_HIDDEN, THUMBBUTTON, TaskbarList,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, RegisterWindowMessageW, SM_CXSMICON,
        WM_COMMAND, WM_NCDESTROY,
    };
    use windows::core::w;

    use super::{SLOTS, TaskbarState, appearance, clicked_slot, push_click, wake};
    use crate::settings::TaskbarButton;
    use crate::theme::Icon;

    const SUBCLASS_ID: usize = 0x6661_7374;

    struct Applied {
        buttons: Vec<TaskbarButton>,
        state: TaskbarState,
        size: i32,
        dark_taskbar: bool,
    }

    impl Applied {
        fn matches(
            &self,
            buttons: &[TaskbarButton],
            state: TaskbarState,
            size: i32,
            dark_taskbar: bool,
        ) -> bool {
            self.buttons == buttons
                && self.state == state
                && self.size == size
                && self.dark_taskbar == dark_taskbar
        }
    }

    struct Attached {
        hwnd: HWND,
        added: bool,
        applied: Option<Applied>,
        reported: bool,
    }

    thread_local! {
        static ATTACHED: RefCell<Option<Attached>> = const { RefCell::new(None) };
        static TASKBAR: RefCell<Option<ITaskbarList3>> = const { RefCell::new(None) };
        static ICONS: RefCell<HashMap<(Icon, i32, bool), HICON>> = RefCell::new(HashMap::new());
        static CREATED: Cell<u32> = const { Cell::new(0) };
    }

    pub fn attach(
        ctx: &egui::Context,
        window: isize,
        buttons: &[TaskbarButton],
        state: TaskbarState,
    ) {
        let hwnd = HWND(window as *mut std::ffi::c_void);
        if hwnd.0.is_null() {
            return;
        }
        subclass(hwnd);
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let dark_taskbar = ctx
            .system_theme()
            .is_none_or(|theme| theme == egui::Theme::Dark);
        let (added, unchanged) = ATTACHED.with(|cell| {
            cell.borrow()
                .as_ref()
                .map(|attached| {
                    let applied = attached
                        .applied
                        .as_ref()
                        .is_some_and(|applied| applied.matches(buttons, state, size, dark_taskbar));
                    (attached.added, applied)
                })
                .unwrap_or((false, false))
        });
        if unchanged {
            return;
        }
        let Some(thumbs) = thumbs(ctx, buttons, state, size, dark_taskbar) else {
            return;
        };
        let Some(taskbar) = taskbar() else {
            return;
        };
        let outcome = unsafe {
            if added {
                taskbar.ThumbBarUpdateButtons(hwnd, &thumbs)
            } else {
                taskbar.ThumbBarAddButtons(hwnd, &thumbs)
            }
        };
        let report = ATTACHED.with(|cell| {
            let mut held = cell.borrow_mut();
            let attached = held.as_mut()?;
            match &outcome {
                Ok(()) => {
                    attached.added = true;
                    attached.applied = Some(Applied {
                        buttons: buttons.to_vec(),
                        state,
                        size,
                        dark_taskbar,
                    });
                    None
                }
                Err(error) => {
                    attached.added = !added;
                    (!std::mem::replace(&mut attached.reported, true)).then(|| error.to_string())
                }
            }
        });
        if let Some(error) = report {
            log::warn!("the taskbar would not take the thumbnail buttons: {error}");
        }
    }

    fn subclass(hwnd: HWND) {
        if ATTACHED.with(|cell| cell.borrow().as_ref().map(|attached| attached.hwnd)) == Some(hwnd)
        {
            return;
        }
        unsafe {
            let _ = SetWindowSubclass(hwnd, Some(handle_message), SUBCLASS_ID, 0);
        }
        ATTACHED.with(|cell| {
            *cell.borrow_mut() = Some(Attached {
                hwnd,
                added: false,
                applied: None,
                reported: false,
            });
        });
    }

    fn taskbar() -> Option<ITaskbarList3> {
        if let Some(taskbar) = TASKBAR.with(|cell| cell.borrow().clone()) {
            return Some(taskbar);
        }
        let taskbar: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }.ok()?;
        unsafe { taskbar.HrInit() }.ok()?;
        TASKBAR.with(|cell| *cell.borrow_mut() = Some(taskbar.clone()));
        Some(taskbar)
    }

    fn thumbs(
        ctx: &egui::Context,
        buttons: &[TaskbarButton],
        state: TaskbarState,
        size: i32,
        dark: bool,
    ) -> Option<Vec<THUMBBUTTON>> {
        let mut thumbs = Vec::with_capacity(SLOTS);
        for slot in 0..SLOTS {
            let mut thumb = THUMBBUTTON {
                iId: slot as u32,
                dwMask: THB_FLAGS,
                dwFlags: THBF_HIDDEN,
                ..Default::default()
            };
            if let Some(button) = buttons.get(slot) {
                let look = appearance(*button, state);
                thumb.dwMask = THB_ICON | THB_TOOLTIP | THB_FLAGS;
                thumb.hIcon = icon(ctx, look.icon, size, dark)?;
                for (target, value) in thumb.szTip.iter_mut().zip(look.tip.encode_utf16()) {
                    *target = value;
                }
                thumb.dwFlags = if look.enabled {
                    THBF_ENABLED
                } else {
                    THBF_DISABLED
                };
            }
            thumbs.push(thumb);
        }
        Some(thumbs)
    }

    fn icon(ctx: &egui::Context, wanted: Icon, size: i32, dark: bool) -> Option<HICON> {
        if let Some(handle) = ICONS.with(|cache| cache.borrow().get(&(wanted, size, dark)).copied())
        {
            return Some(handle);
        }
        let poll = ctx
            .try_load_image(wanted.uri(), SizeHint::Width(size as u32))
            .ok()?;
        let ImagePoll::Ready { image } = poll else {
            return None;
        };
        let handle = create_icon(&image, dark)?;
        ICONS.with(|cache| cache.borrow_mut().insert((wanted, size, dark), handle));
        Some(handle)
    }

    fn create_icon(image: &egui::ColorImage, dark: bool) -> Option<HICON> {
        let [width, height] = image.size;
        if width == 0 || height == 0 {
            return None;
        }
        let mut pixels = Vec::with_capacity(width * height * 4);
        for pixel in &image.pixels {
            let alpha = pixel.a();
            let level = if dark { alpha } else { 0 };
            pixels.extend_from_slice(&[level, level, level, alpha]);
        }
        unsafe {
            let color = CreateBitmap(
                width as i32,
                height as i32,
                1,
                32,
                Some(pixels.as_ptr().cast()),
            );
            if color.is_invalid() {
                return None;
            }
            let mask = CreateBitmap(width as i32, height as i32, 1, 1, None);
            let info = ICONINFO {
                fIcon: true.into(),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: color,
            };
            let handle = CreateIconIndirect(&info).ok();
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            handle
        }
    }

    fn created_message() -> u32 {
        CREATED.with(|cell| {
            if cell.get() == 0 {
                cell.set(unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) });
            }
            cell.get()
        })
    }

    unsafe extern "system" fn handle_message(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if message == WM_COMMAND {
            if let Some(slot) = clicked_slot(wparam.0) {
                push_click(slot);
                return LRESULT(0);
            }
        } else if message == created_message() {
            ATTACHED.with(|cell| {
                if let Some(attached) = cell.borrow_mut().as_mut() {
                    attached.applied = None;
                }
            });
            wake();
        } else if message == WM_NCDESTROY {
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(handle_message), SUBCLASS_ID);
            }
            ATTACHED.with(|cell| *cell.borrow_mut() = None);
        }
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::TaskbarButton;
    use crate::theme::Icon;

    fn state() -> TaskbarState {
        TaskbarState {
            playing: false,
            saved: false,
            repeat_one: false,
            has_track: true,
        }
    }

    #[test]
    fn a_thumb_button_click_decodes_to_its_slot() {
        assert_eq!(clicked_slot(0x1800_0000), Some(0));
        assert_eq!(clicked_slot(0x1800_0004), Some(4));
    }

    #[test]
    fn other_window_commands_are_not_thumb_button_clicks() {
        assert_eq!(clicked_slot(0x0000_0002), None);
        assert_eq!(clicked_slot(0x1801_0002), None);
    }

    #[test]
    fn a_click_past_the_last_slot_is_ignored() {
        assert_eq!(clicked_slot(0x1800_0005), None);
    }

    fn order() -> Vec<TaskbarButton> {
        TaskbarButton::ALL.to_vec()
    }

    #[test]
    fn dragging_a_button_down_leaves_it_in_the_slot_it_was_dropped_in() {
        let mut buttons = order();
        reorder(&mut buttons, 0, 3);
        assert_eq!(
            buttons,
            vec![
                TaskbarButton::Previous,
                TaskbarButton::PlayPause,
                TaskbarButton::Like,
                TaskbarButton::Next,
                TaskbarButton::RepeatOne,
            ]
        );
    }

    #[test]
    fn dragging_a_button_up_leaves_it_in_the_slot_it_was_dropped_in() {
        let mut buttons = order();
        reorder(&mut buttons, 4, 1);
        assert_eq!(
            buttons,
            vec![
                TaskbarButton::Like,
                TaskbarButton::RepeatOne,
                TaskbarButton::Previous,
                TaskbarButton::PlayPause,
                TaskbarButton::Next,
            ]
        );
    }

    #[test]
    fn dropping_a_button_back_where_it_started_changes_nothing() {
        let mut buttons = order();
        reorder(&mut buttons, 2, 2);
        assert_eq!(buttons, order());
        reorder(&mut buttons, 2, 3);
        assert_eq!(buttons, order());
    }

    #[test]
    fn a_drag_from_outside_the_list_is_ignored() {
        let mut buttons = order();
        reorder(&mut buttons, 9, 1);
        assert_eq!(buttons, order());
    }

    #[test]
    fn the_play_button_follows_playback() {
        let stopped = state();
        let playing = TaskbarState {
            playing: true,
            ..stopped
        };
        assert_eq!(
            appearance(TaskbarButton::PlayPause, stopped).icon,
            Icon::PlayFilled
        );
        assert_eq!(
            appearance(TaskbarButton::PlayPause, playing).icon,
            Icon::PauseFilled
        );
    }

    #[test]
    fn the_like_button_follows_the_saved_state() {
        let unsaved = state();
        let saved = TaskbarState {
            saved: true,
            ..unsaved
        };
        assert_eq!(appearance(TaskbarButton::Like, unsaved).icon, Icon::Heart);
        assert_eq!(
            appearance(TaskbarButton::Like, saved).icon,
            Icon::HeartFilled
        );
    }

    #[test]
    fn the_repeat_button_follows_repeat_one() {
        let off = state();
        let on = TaskbarState {
            repeat_one: true,
            ..off
        };
        assert_eq!(appearance(TaskbarButton::RepeatOne, off).icon, Icon::Repeat);
        assert_eq!(appearance(TaskbarButton::RepeatOne, on).icon, Icon::Repeat1);
    }

    #[test]
    fn the_buttons_that_need_a_track_are_disabled_without_one() {
        let empty = TaskbarState {
            has_track: false,
            ..state()
        };
        assert!(!appearance(TaskbarButton::Like, empty).enabled);
        assert!(!appearance(TaskbarButton::RepeatOne, empty).enabled);
        assert!(appearance(TaskbarButton::PlayPause, empty).enabled);
        assert!(appearance(TaskbarButton::Next, empty).enabled);
        assert!(appearance(TaskbarButton::Previous, empty).enabled);
    }

    #[test]
    fn clicks_queue_up_in_order_and_wake_the_interface() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        set_waker(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        push_click(3);
        push_click(0);
        assert_eq!(drain_clicks(), vec![3, 0]);
        assert!(drain_clicks().is_empty());
        assert_eq!(woken.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_play_button_names_what_it_will_do() {
        let stopped = state();
        let playing = TaskbarState {
            playing: true,
            ..stopped
        };
        assert_eq!(appearance(TaskbarButton::PlayPause, stopped).tip, "Play");
        assert_eq!(appearance(TaskbarButton::PlayPause, playing).tip, "Pause");
    }
}
