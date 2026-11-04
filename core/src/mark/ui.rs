//! Panels, the menu bar item and the window list for the macOS marker.

use super::*;

/// A borderless panel that never takes focus. `mouse`: it takes clicks.
pub(super) fn panel(mtm: MainThreadMarker, mouse: bool) -> Retained<NSPanel> {
    let p = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0)),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: the marker keeps the panel in a Retained.
    unsafe { p.setReleasedWhenClosed(false) };
    p.setOpaque(false);
    p.setBackgroundColor(Some(&NSColor::clearColor()));
    p.setHasShadow(false);
    p.setIgnoresMouseEvents(!mouse);
    p.setHidesOnDeactivate(false);
    p.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    p
}

pub(super) fn ns_rect(r: Rect) -> NSRect {
    NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h))
}

pub(super) fn main_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm)
        .firstObject()
        .map(|s| s.frame().size.height)
        .unwrap_or(0.0)
}

/// The visible part of each screen, in window server coordinates.
pub(super) fn visible_screens(mtm: MainThreadMarker, main_h: f64) -> Vec<Rect> {
    NSScreen::screens(mtm)
        .iter()
        .map(|s| {
            let v = s.visibleFrame();
            Rect::new(v.origin.x, v.origin.y, v.size.width, v.size.height).flip(main_h)
        })
        .collect()
}

/// The window right in front of `id` in the list, front first.
pub(super) fn above_of(wins: &[Win], id: u32) -> Option<u32> {
    let i = wins.iter().position(|w| w.id == id)?;
    i.checked_sub(1).map(|j| wins[j].id)
}

/// The windows on screen, front first, from the window server.
pub(super) fn window_list() -> Vec<Win> {
    let opts = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(opts, 0) else {
        return Vec::new();
    };
    // SAFETY: CFArray and NSArray are toll-free bridged. The list holds
    // dictionaries with the documented keys and number values.
    let arr: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*list as *const CFArray).cast() };
    let num = |d: &NSDictionary<NSString, AnyObject>, k: &NSString| -> Option<f64> {
        let v = d.objectForKey(k)?;
        Some(unsafe { msg_send![&*v, doubleValue] })
    };
    let mut out = Vec::new();
    for d in arr.iter() {
        let (Some(id), Some(pid)) = (
            num(&d, ns_string!("kCGWindowNumber")),
            num(&d, ns_string!("kCGWindowOwnerPID")),
        ) else {
            continue;
        };
        let Some(b) = d.objectForKey(ns_string!("kCGWindowBounds")) else {
            continue;
        };
        let field = |k: &NSString| -> f64 {
            let v: Option<Retained<AnyObject>> = unsafe { msg_send![&*b, objectForKey: k] };
            v.map(|v| unsafe { msg_send![&*v, doubleValue] })
                .unwrap_or(0.0)
        };
        out.push(Win {
            id: id as u32,
            pid: pid as u32,
            layer: num(&d, ns_string!("kCGWindowLayer")).unwrap_or(0.0) as i64,
            bounds: Rect::new(
                field(ns_string!("X")),
                field(ns_string!("Y")),
                field(ns_string!("Width")),
                field(ns_string!("Height")),
            ),
            alpha: num(&d, ns_string!("kCGWindowAlpha")).unwrap_or(1.0),
        });
    }
    out
}

/// Each process and its parent, from the kernel.
pub(super) fn proc_table() -> crate::mark::ProcTable {
    let mut out = BTreeMap::new();
    // SAFETY: proc_listallpids fills at most the given number of bytes.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return out;
    }
    let mut pids = vec![0 as libc::pid_t; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    for &pid in pids.iter().take(n.max(0) as usize) {
        if pid <= 0 {
            continue;
        }
        // SAFETY: proc_bsdinfo is plain data, and the size matches.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if r == size {
            out.insert(pid as u32, info.pbi_ppid);
        }
    }
    out
}

unsafe extern "C-unwind" fn ax_changed(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    _notification: NonNull<CFString>,
    _refcon: *mut c_void,
) {
    DIRTY.store(true, Ordering::Relaxed);
}

/// An observer of the window moves of one app, on the main run loop.
pub(super) fn ax_observer(pid: u32) -> Option<CFRetained<AXObserver>> {
    let mut raw: *mut AXObserver = std::ptr::null_mut();
    // SAFETY: the callback has the documented signature, and `raw` is a
    // valid out pointer.
    let err = unsafe { AXObserver::create(pid as i32, Some(ax_changed), NonNull::from(&mut raw)) };
    if err != AXError::Success {
        return None;
    }
    // SAFETY: AXObserverCreate returns a +1 reference.
    let obs = unsafe { CFRetained::from_raw(NonNull::new(raw)?) };
    let app = unsafe { AXUIElement::new_application(pid as i32) };
    for n in [
        "AXWindowMoved",
        "AXWindowResized",
        "AXWindowMiniaturized",
        "AXWindowDeminiaturized",
        "AXWindowCreated",
        "AXFocusedWindowChanged",
        "AXUIElementDestroyed",
    ] {
        let name = CFString::from_static_str(n);
        // SAFETY: a valid element and notification name, no refcon.
        let _ = unsafe { obs.add_notification(&app, &name, std::ptr::null_mut()) };
    }
    let src = unsafe { obs.run_loop_source() };
    CFRunLoop::main()?.add_source(Some(&src), unsafe { kCFRunLoopCommonModes });
    Some(obs)
}

/// Raise the window of `pid` whose frame is `bounds`, and bring its app to
/// the front.
///
/// # Safety
/// Calls the Accessibility API, which needs the permission.
pub(super) unsafe fn ax_raise(pid: u32, bounds: Rect) {
    let app = unsafe { AXUIElement::new_application(pid as i32) };
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe {
        app.copy_attribute_value(
            &CFString::from_static_str("AXWindows"),
            NonNull::from(&mut value),
        )
    };
    if err != AXError::Success || value.is_null() {
        return;
    }
    let windows: CFRetained<CFArray> =
        unsafe { CFRetained::from_raw(NonNull::new_unchecked(value as *mut CFArray)) };
    let count = windows.count();
    for i in 0..count {
        let w = unsafe { windows.value_at_index(i) } as *const AXUIElement;
        let Some(w) = (unsafe { w.as_ref() }) else {
            continue;
        };
        let Some(frame) = (unsafe { ax_frame(w) }) else {
            continue;
        };
        if frame.near(bounds, 2.0) {
            let _ = unsafe { w.perform_action(&CFString::from_static_str("AXRaise")) };
            break;
        }
    }
    if let Some(t) = unsafe { kCFBooleanTrue } {
        let _ = unsafe { app.set_attribute_value(&CFString::from_static_str("AXFrontmost"), t) };
    }
}

/// The frame of an Accessibility window element, in window server
/// coordinates.
unsafe fn ax_frame(w: &AXUIElement) -> Option<Rect> {
    let get = |name: &'static str| -> Option<CFRetained<AXValue>> {
        let mut v: *const CFType = std::ptr::null();
        let err = unsafe {
            w.copy_attribute_value(&CFString::from_static_str(name), NonNull::from(&mut v))
        };
        if err != AXError::Success || v.is_null() {
            return None;
        }
        Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(v as *mut AXValue)) })
    };
    let mut p = CGPoint::new(0.0, 0.0);
    let mut s = CGSize::new(0.0, 0.0);
    let pos = get("AXPosition")?;
    let size = get("AXSize")?;
    let ok = unsafe {
        pos.value(AXValueType::CGPoint, NonNull::from(&mut p).cast())
            && size.value(AXValueType::CGSize, NonNull::from(&mut s).cast())
    };
    ok.then(|| Rect::new(p.x, p.y, s.width, s.height))
}

/// A menu item. Without an action it is a line of text.
pub(super) fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<objc2::runtime::Sel>,
    tag: isize,
    target: &AnyObject,
) -> Retained<NSMenuItem> {
    // SAFETY: the target answers the action and outlives the menu.
    let it = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            ns_string!(""),
        )
    };
    if action.is_some() {
        unsafe { it.setTarget(Some(target)) };
        it.setTag(tag);
    } else {
        it.setEnabled(false);
    }
    it
}

/// A small dot in the color of a tone, for a menu item.
pub(super) fn dot_image(tone: Tone) -> Option<Retained<NSImage>> {
    let (r, g, b) = tone.rgb();
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        ns_string!("circle.fill"),
        Some(&NSString::from_str(tone.meaning())),
    )?;
    // SAFETY: a constant of AppKit.
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(8.0, unsafe {
        NSFontWeightRegular
    });
    let cfg = size.configurationByApplyingConfiguration(
        &NSImageSymbolConfiguration::configurationWithHierarchicalColor(&color),
    );
    img.imageWithSymbolConfiguration(&cfg)
}

/// The count on the menu bar item, and its menu: one entry per protected
/// window. The menu is built again only when its text changes.
pub(super) fn update_menu(st: &mut State, mtm: MainThreadMarker, me: &AnyObject) {
    let now = crate::state::now();
    let mut per_run: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (id, m) in &st.marks {
        per_run.entry(m.run).or_default().push(*id);
    }
    let mut entries: Vec<(String, Option<u32>, Tone)> = Vec::new();
    for (i, r) in st.runs.iter().enumerate() {
        let tone = Tone::of(&r.network);
        match per_run.get(&i) {
            Some(ids) => {
                for (n, id) in ids.iter().enumerate() {
                    entries.push((
                        crate::mark::menu_title(r, now, n + 1, ids.len()),
                        Some(*id),
                        tone,
                    ));
                }
            }
            None => entries.push((
                format!(
                    "{} · no window on this screen",
                    crate::mark::menu_title(r, now, 1, 1)
                ),
                None,
                tone,
            )),
        }
    }
    // SAFETY: a plain query.
    let trusted = unsafe { AXIsProcessTrusted() };
    let sig = format!("{entries:?}{trusted}");
    if sig == st.menu_sig {
        return;
    }
    st.menu_sig = sig;
    let count = st.marks.len();
    if let Some(button) = st.status.button(mtm) {
        button.setTitle(&NSString::from_str(&format!(" {count}")));
        button.setToolTip(Some(&NSString::from_str(&format!(
            "paddock protects {count} window{}",
            if count == 1 { "" } else { "s" }
        ))));
    }
    let menu = &st.menu;
    menu.removeAllItems();
    let head = match count {
        1 => "paddock protects 1 window".to_string(),
        n => format!("paddock protects {n} windows"),
    };
    menu.addItem(&item(mtm, &head, None, 0, me));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, id, tone) in &entries {
        let it = match id {
            Some(id) => item(mtm, title, Some(sel!(raise:)), *id as isize, me),
            None => item(mtm, title, None, 0, me),
        };
        if let Some(img) = dot_image(*tone) {
            it.setImage(Some(&img));
        }
        menu.addItem(&it);
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    if !trusted {
        menu.addItem(&item(
            mtm,
            "Allow paddock to raise windows (Accessibility)…",
            Some(sel!(openAccessibility:)),
            0,
            me,
        ));
    }
    menu.addItem(&item(
        mtm,
        "Hide the marks until the next start",
        Some(sel!(quit:)),
        0,
        me,
    ));
}
