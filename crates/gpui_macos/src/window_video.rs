//! The video backdrop of a wallpaper-mode window: a looping, muted video blurred like the
//! wallpaper picture and placed the same way, which plays only while someone can see it.
//!
//! Windows showing the same file share one player, so a window laid over another (a floating
//! panel) shows exactly the same frame as the window under it, and the video is decoded once.

use cocoa::{
    appkit::{NSView, NSViewHeightSizable, NSViewWidthSizable, NSWindowOrderingMode},
    base::{id, nil},
    foundation::NSRect,
};
use core_foundation::{
    base::{CFRelease, CFTypeRef, TCFType},
    string::{CFString, CFStringRef},
};
use core_foundation_sys::runloop::{
    CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopSourceRef, kCFRunLoopDefaultMode,
};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Sel, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;
use std::{
    ffi::c_void,
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{ns_string, window_wallpaper};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVLayerVideoGravityResizeAspectFill: id;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMTimeZero: CMTime;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSApplicationDidBecomeActiveNotification: id;
    static NSApplicationDidResignActiveNotification: id;
    static NSApplicationDidChangeScreenParametersNotification: id;
    static NSWindowDidChangeOcclusionStateNotification: id;
    static NSWindowDidMiniaturizeNotification: id;
    static NSWindowDidDeminiaturizeNotification: id;
    static NSWorkspaceScreensDidSleepNotification: id;
    static NSWorkspaceScreensDidWakeNotification: id;
    static NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification: id;
    static NSWorkspaceActiveSpaceDidChangeNotification: id;
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSProcessInfoPowerStateDidChangeNotification: id;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFStringRef;
    fn IOPSNotificationCreateRunLoopSource(
        callback: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> CFRunLoopSourceRef;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// Matches the live backdrop's blur (`BLURRED_VIEW_BLUR_RADIUS` in window.rs), in points.
const VIDEO_BLUR_RADIUS: f64 = 60.0;

/// The video layer reaches this far past every edge of the area it covers, so the blur's
/// transparent fringe falls outside the window instead of darkening its edges.
const VIDEO_BLEED: f64 = VIDEO_BLUR_RADIUS * 2.0;

static VIEW_CLASS: OnceLock<usize> = OnceLock::new();

/// Every live video backdrop view.
static VIEWS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// One player per file, shared by every view showing it: (path, AVQueuePlayer, AVPlayerLooper,
/// views using it). The player and looper each hold one retain.
static PLAYERS: Mutex<Vec<(String, usize, usize, usize)>> = Mutex::new(Vec::new());

/// Whether the displays are asleep, from the workspace's sleep and wake notifications.
static SCREENS_ASLEEP: AtomicBool = AtomicBool::new(false);

static POWER_SOURCE_OBSERVED: OnceLock<()> = OnceLock::new();

/// Adds a video backdrop playing `path` below everything in `content_view`, or `None` when the
/// file does not exist.
pub(crate) unsafe fn create_view(content_view: id, path: &Path, only_on_power: bool) -> Option<id> {
    unsafe {
        let path_string = path.to_str()?.to_string();
        if !path.is_file() {
            return None;
        }
        let player = acquire_player(&path_string)?;
        let class = view_class();
        let view: id = msg_send![class, alloc];
        let view = NSView::initWithFrame_(view, NSView::bounds(content_view));
        view.setAutoresizingMask_(NSViewWidthSizable | NSViewHeightSizable);
        let _: () = msg_send![view, setWantsLayer: YES];
        // Layer filters only run on a view that asks for them.
        let _: () = msg_send![view, setLayerUsesCoreImageFilters: YES];
        let layer: id = msg_send![view, layer];
        let black: id = msg_send![class!(NSColor), blackColor];
        let black: id = msg_send![black, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: black];
        // The blur's bleed reaches past the view; keep it inside.
        let _: () = msg_send![layer, setMasksToBounds: YES];

        let video_layer: id = msg_send![class!(AVPlayerLayer), playerLayerWithPlayer: player];
        let _: () = msg_send![video_layer, setVideoGravity: AVLayerVideoGravityResizeAspectFill];
        // `window_wallpaper::layout` places the sublayer with this name.
        let _: () = msg_send![video_layer, setName: ns_string("wallpaper")];
        let blur: id = msg_send![class!(CIFilter), filterWithName: ns_string("CIGaussianBlur")];
        let _: () = msg_send![blur, setDefaults];
        let radius: id = msg_send![class!(NSNumber), numberWithDouble: VIDEO_BLUR_RADIUS];
        let _: () = msg_send![blur, setValue: radius forKey: ns_string("inputRadius")];
        let filters: id = msg_send![class!(NSArray), arrayWithObject: blur];
        let _: () = msg_send![video_layer, setFilters: filters];
        let _: () = msg_send![layer, addSublayer: video_layer];

        let object = &mut *(view as *mut Object);
        object.set_ivar::<id>("gpuiPlayer", player);
        object.set_ivar::<BOOL>("gpuiOnlyOnPower", if only_on_power { YES } else { NO });
        object.set_ivar::<BOOL>("gpuiWantsPlay", NO);
        let path_ns: id = msg_send![ns_string(&path_string), retain];
        object.set_ivar::<id>("gpuiPath", path_ns);

        let _: () = msg_send![
            content_view,
            addSubview: view
            positioned: NSWindowOrderingMode::NSWindowBelow
            relativeTo: nil
        ];
        VIEWS.lock().push(view as usize);
        observe_conditions(view);
        observe_power_source();
        refresh(view);
        let _: id = msg_send![view, autorelease];
        Some(view)
    }
}

pub(crate) unsafe fn remove_view(view: id) {
    unsafe {
        detach(view);
        NSView::removeFromSuperview(view);
    }
}

/// Stops `view` observing and hands back its share of the player. Runs once: from `remove_view`,
/// or from the view's dealloc when its window went away without removing it.
unsafe fn detach(view: id) {
    unsafe {
        let registered = {
            let mut views = VIEWS.lock();
            let before = views.len();
            views.retain(|existing| *existing != view as usize);
            views.len() != before
        };
        if !registered {
            return;
        }
        stop_observing_conditions(view);
        let object = &mut *(view as *mut Object);
        object.set_ivar::<BOOL>("gpuiWantsPlay", NO);
        let path: id = *object.get_ivar::<id>("gpuiPath");
        let player: id = *object.get_ivar::<id>("gpuiPlayer");
        if let Some(path) = ns_to_string(path) {
            update_player(player);
            release_player(&path);
        }
        let _: () = msg_send![path, release];
        object.set_ivar::<id>("gpuiPath", nil);
    }
}

extern "C" fn dealloc_view(this: &Object, _: Sel) {
    unsafe {
        let this_id = this as *const Object as id;
        detach(this_id);
        let _: () = msg_send![super(this, class!(NSView)), dealloc];
    }
}

/// The file `view` plays.
pub(crate) unsafe fn view_path(view: id) -> Option<String> {
    unsafe {
        let object = &*(view as *const Object);
        ns_to_string(*object.get_ivar::<id>("gpuiPath"))
    }
}

pub(crate) unsafe fn set_only_on_power(view: id, only_on_power: bool) {
    unsafe {
        let object = &mut *(view as *mut Object);
        object.set_ivar::<BOOL>("gpuiOnlyOnPower", if only_on_power { YES } else { NO });
        refresh(view);
    }
}

/// Places the video like the wallpaper picture, then lets it reach past every edge by the blur's
/// bleed so the edges stay as bright as the middle.
pub(crate) unsafe fn layout(
    view: id,
    window: id,
    screen_frame: NSRect,
    follows_screen: bool,
    cover: Option<NSRect>,
) {
    unsafe {
        window_wallpaper::layout(view, window, screen_frame, follows_screen, cover);
        let layer: id = msg_send![view, layer];
        let sublayers: id = msg_send![layer, sublayers];
        if sublayers == nil {
            return;
        }
        let video_layer: id = msg_send![sublayers, firstObject];
        if video_layer == nil {
            return;
        }
        let frame: NSRect = msg_send![video_layer, frame];
        let bled = NSRect::new(
            cocoa::foundation::NSPoint::new(
                frame.origin.x - VIDEO_BLEED,
                frame.origin.y - VIDEO_BLEED,
            ),
            cocoa::foundation::NSSize::new(
                frame.size.width + VIDEO_BLEED * 2.0,
                frame.size.height + VIDEO_BLEED * 2.0,
            ),
        );
        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
        // The picture layout resizes with the view on its own; the bled frame is set by hand, so
        // it is redone on every layout (`window_did_resize` calls back here).
        let _: () = msg_send![video_layer, setAutoresizingMask: 0u32];
        let _: () = msg_send![video_layer, setFrame: bled];
        let _: () = msg_send![class!(CATransaction), commit];
    }
}

unsafe fn acquire_player(path: &str) -> Option<id> {
    unsafe {
        let mut players = PLAYERS.lock();
        if let Some(entry) = players.iter_mut().find(|(existing, ..)| existing == path) {
            entry.3 += 1;
            return Some(entry.1 as id);
        }
        let url: id = msg_send![class!(NSURL), fileURLWithPath: ns_string(path)];
        let item: id = msg_send![class!(AVPlayerItem), playerItemWithURL: url];
        if item == nil {
            return None;
        }
        let player: id = msg_send![class!(AVQueuePlayer), alloc];
        let player: id = msg_send![player, init];
        if player == nil {
            return None;
        }
        let _: () = msg_send![player, setMuted: YES];
        // A background must never keep the display awake or show up as media playing.
        let _: () = msg_send![player, setPreventsDisplaySleepDuringVideoPlayback: NO];
        let looper: id = msg_send![
            class!(AVPlayerLooper),
            playerLooperWithPlayer: player
            templateItem: item
        ];
        let looper: id = msg_send![looper, retain];
        let _: () = msg_send![player, pause];
        players.push((path.to_string(), player as usize, looper as usize, 1));
        Some(player)
    }
}

unsafe fn release_player(path: &str) {
    unsafe {
        let mut players = PLAYERS.lock();
        let Some(index) = players.iter().position(|(existing, ..)| existing == path) else {
            return;
        };
        players[index].3 -= 1;
        if players[index].3 > 0 {
            return;
        }
        let (_, player, looper, _) = players.remove(index);
        let player = player as id;
        let looper = looper as id;
        let _: () = msg_send![player, pause];
        let _: () = msg_send![looper, disableLooping];
        let _: () = msg_send![looper, release];
        let _: () = msg_send![player, release];
    }
}

/// Works out whether `view` can be seen and should move, then plays or pauses its player.
unsafe fn refresh(view: id) {
    unsafe {
        let wants = view_wants_play(view);
        let object = &mut *(view as *mut Object);
        object.set_ivar::<BOOL>("gpuiWantsPlay", if wants { YES } else { NO });
        let player: id = *object.get_ivar::<id>("gpuiPlayer");
        update_player(player);
    }
}

/// Plays `player` while any view showing it wants to, and pauses it otherwise. Under Reduce Motion
/// the paused video goes back to its first frame, so the window shows a still picture.
unsafe fn update_player(player: id) {
    unsafe {
        let wants = VIEWS.lock().iter().any(|view| {
            let object = &*(*view as *const Object);
            *object.get_ivar::<id>("gpuiPlayer") == player
                && *object.get_ivar::<BOOL>("gpuiWantsPlay") == YES
        });
        let rate: f32 = msg_send![player, rate];
        if wants {
            if rate == 0.0 {
                let _: () = msg_send![player, play];
            }
        } else {
            if rate != 0.0 {
                let _: () = msg_send![player, pause];
            }
            if reduce_motion() {
                let _: () = msg_send![player, seekToTime: kCMTimeZero];
            }
        }
    }
}

unsafe fn view_wants_play(view: id) -> bool {
    unsafe {
        let window: id = msg_send![view, window];
        let object = &*(view as *const Object);
        let only_on_power = *object.get_ivar::<BOOL>("gpuiOnlyOnPower") == YES;
        window_may_animate(window, only_on_power)
    }
}

/// Whether a moving backdrop in `window` may move right now: someone can see it, and nothing asks
/// it to hold still. The live backdrop (window_live.rs) moves under exactly the same rules.
pub(crate) unsafe fn window_may_animate(window: id, only_on_power: bool) -> bool {
    unsafe {
        if window == nil {
            return false;
        }
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let active: BOOL = msg_send![app, isActive];
        let miniaturized: BOOL = msg_send![window, isMiniaturized];
        // NSWindowOcclusionStateVisible
        let occlusion: u64 = msg_send![window, occlusionState];
        let visible = occlusion & (1 << 1) != 0;
        let process: id = msg_send![class!(NSProcessInfo), processInfo];
        let low_power: BOOL = msg_send![process, isLowPowerModeEnabled];
        active == YES
            && miniaturized == NO
            && visible
            && !SCREENS_ASLEEP.load(Ordering::Relaxed)
            && low_power == NO
            && !reduce_motion()
            && (!only_on_power || on_external_power())
    }
}

pub(crate) unsafe fn reduce_motion() -> bool {
    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let reduce: BOOL = msg_send![workspace, accessibilityDisplayShouldReduceMotion];
        reduce == YES
    }
}

/// Whether the computer is running on a charger rather than its battery. A desktop always is.
fn on_external_power() -> bool {
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return true;
        }
        let source = IOPSGetProvidingPowerSourceType(snapshot);
        let on_power = source.is_null()
            || CFString::wrap_under_get_rule(source).to_string() != "Battery Power";
        CFRelease(snapshot);
        on_power
    }
}

fn view_class() -> *const Class {
    *VIEW_CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("GPUIVideoBackdropView", class!(NSView)).unwrap();
        decl.add_ivar::<id>("gpuiPlayer");
        decl.add_ivar::<id>("gpuiPath");
        decl.add_ivar::<BOOL>("gpuiOnlyOnPower");
        decl.add_ivar::<BOOL>("gpuiWantsPlay");
        unsafe {
            decl.add_method(
                sel!(videoConditionsDidChange:),
                video_conditions_did_change as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(refreshVideoPlayback),
                refresh_video_playback as extern "C" fn(&Object, Sel),
            );
            decl.add_method(
                sel!(viewDidMoveToWindow),
                view_did_move_to_window as extern "C" fn(&Object, Sel),
            );
            decl.add_method(sel!(dealloc), dealloc_view as extern "C" fn(&Object, Sel));
        }
        decl.register() as *const Class as usize
    }) as *const Class
}

/// Everything that decides whether the video can be seen or should move: the app coming to or
/// leaving the front, a window hidden, covered or minimised, the displays sleeping, Low Power
/// Mode, Reduce Motion. Screens and Spaces changing re-place the video like the picture.
pub(crate) unsafe fn observe_conditions(view: id) {
    unsafe {
        let center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
        for name in [
            NSApplicationDidBecomeActiveNotification,
            NSApplicationDidResignActiveNotification,
            NSApplicationDidChangeScreenParametersNotification,
            NSWindowDidChangeOcclusionStateNotification,
            NSWindowDidMiniaturizeNotification,
            NSWindowDidDeminiaturizeNotification,
            NSProcessInfoPowerStateDidChangeNotification,
        ] {
            let _: () = msg_send![
                center,
                addObserver: view
                selector: sel!(videoConditionsDidChange:)
                name: name
                object: nil
            ];
        }
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let workspace_center: id = msg_send![workspace, notificationCenter];
        for name in [
            NSWorkspaceScreensDidSleepNotification,
            NSWorkspaceScreensDidWakeNotification,
            NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
            NSWorkspaceActiveSpaceDidChangeNotification,
        ] {
            let _: () = msg_send![
                workspace_center,
                addObserver: view
                selector: sel!(videoConditionsDidChange:)
                name: name
                object: nil
            ];
        }
    }
}

pub(crate) unsafe fn stop_observing_conditions(view: id) {
    unsafe {
        let center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![center, removeObserver: view];
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let workspace_center: id = msg_send![workspace, notificationCenter];
        let _: () = msg_send![workspace_center, removeObserver: view];
    }
}

/// Plugging in or unplugging the charger, through IOKit's power source notification on the main
/// run loop. Registered once for the whole app.
pub(crate) fn observe_power_source() {
    POWER_SOURCE_OBSERVED.get_or_init(|| unsafe {
        let source =
            IOPSNotificationCreateRunLoopSource(power_source_did_change, std::ptr::null_mut());
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode);
        }
    });
}

extern "C" fn power_source_did_change(_: *mut c_void) {
    let views: Vec<usize> = VIEWS.lock().clone();
    for view in views {
        unsafe { refresh(view as id) };
    }
    crate::window_live::refresh_all();
}

extern "C" fn video_conditions_did_change(this: &Object, _: Sel, notification: id) {
    unsafe {
        let this_id = this as *const Object as id;
        note_condition_notification(this_id, notification);
        // Low Power Mode changes arrive on a background queue.
        let main: BOOL = msg_send![class!(NSThread), isMainThread];
        if main == YES {
            refresh(this_id);
        } else {
            let _: () = msg_send![
                this_id,
                performSelectorOnMainThread: sel!(refreshVideoPlayback)
                withObject: nil
                waitUntilDone: NO
            ];
        }
    }
}

/// What every moving backdrop does with one of the notifications `observe_conditions` registers:
/// tracks the displays sleeping, and re-places its window's backdrop when screens or Spaces change.
pub(crate) unsafe fn note_condition_notification(view: id, notification: id) {
    unsafe {
        let name: id = if notification == nil {
            nil
        } else {
            msg_send![notification, name]
        };
        if name != nil {
            let is = |other: id| -> bool {
                let equal: BOOL = msg_send![name, isEqualToString: other];
                equal == YES
            };
            if is(NSWorkspaceScreensDidSleepNotification) {
                SCREENS_ASLEEP.store(true, Ordering::Relaxed);
            } else if is(NSWorkspaceScreensDidWakeNotification) {
                SCREENS_ASLEEP.store(false, Ordering::Relaxed);
            }
            if is(NSApplicationDidChangeScreenParametersNotification)
                || is(NSWorkspaceActiveSpaceDidChangeNotification)
            {
                let _: id = msg_send![view, retain];
                let _: id = msg_send![view, autorelease];
                let window: id = msg_send![view, window];
                if window != nil {
                    crate::window::refresh_window_backdrop(&*window);
                }
            }
        }
    }
}

extern "C" fn refresh_video_playback(this: &Object, _: Sel) {
    unsafe {
        let this_id = this as *const Object as id;
        if VIEWS.lock().contains(&(this_id as usize)) {
            refresh(this_id);
        }
    }
}

extern "C" fn view_did_move_to_window(this: &Object, _: Sel) {
    unsafe {
        let this_id = this as *const Object as id;
        if VIEWS.lock().contains(&(this_id as usize)) {
            refresh(this_id);
        }
    }
}

unsafe fn ns_to_string(string: id) -> Option<String> {
    unsafe {
        if string == nil {
            return None;
        }
        let utf8: *const std::os::raw::c_char = msg_send![string, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned(),
        )
    }
}
