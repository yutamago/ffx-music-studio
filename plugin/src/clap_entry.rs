//! The CLAP entry point: what `nih_export_clap!` makes, with one change.
//! The GUI extension's `show` (and `hide`) answer true.
//!
//! nih-plug only offers embedded editors, and its `clap_plugin_gui.show`
//! returns false ("would not make any sense for embedded editors"). The
//! CLAP spec has `show` report success for every editor, and hosts that
//! check it give up on a false: SoundCraft creates the editor, embeds it
//! in its own window, calls `show`, and destroys it again. So this entry
//! hands out nih-plug's plugin with a `get_extension` that returns a copy of
//! nih-plug's GUI vtable whose `show`/`hide` say yes; the editor itself is
//! already visible once attached (`set_parent`).

use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, OnceLock};

use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui};
use nih_plug::wrapper::clap::{
    CLAP_PLUGIN_FACTORY_ID, CLAP_VERSION, PluginDescriptor, Wrapper, clap_host, clap_plugin, clap_plugin_descriptor, clap_plugin_entry,
    clap_plugin_factory,
};

use crate::plugin::FfxSound;

static DESCRIPTOR: OnceLock<PluginDescriptor> = OnceLock::new();

fn descriptor() -> &'static PluginDescriptor {
    DESCRIPTOR.get_or_init(PluginDescriptor::for_plugin::<FfxSound>)
}

type GetExtension = unsafe extern "C" fn(*const clap_plugin, *const c_char) -> *const c_void;

/// nih-plug's own `get_extension` (the same function for every instance).
static NIH_GET_EXTENSION: OnceLock<GetExtension> = OnceLock::new();

struct Gui(clap_plugin_gui);
// SAFETY: a table of function pointers, never changed after it is made.
unsafe impl Send for Gui {}
unsafe impl Sync for Gui {}

static GUI: OnceLock<Gui> = OnceLock::new();

unsafe extern "C" fn gui_show(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn gui_hide(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn get_extension(plugin: *const clap_plugin, id: *const c_char) -> *const c_void {
    let Some(nih) = NIH_GET_EXTENSION.get() else { return std::ptr::null() };
    // SAFETY: the host's arguments, passed on as nih-plug expects them.
    let ext = unsafe { nih(plugin, id) };
    // SAFETY: `id` is the host's NUL-terminated string (nih-plug checked it for null when it
    // returned something).
    if ext.is_null() || id.is_null() || unsafe { CStr::from_ptr(id) } != CLAP_EXT_GUI {
        return ext;
    }
    let gui = GUI.get_or_init(|| {
        // SAFETY: nih-plug returned its `clap_plugin_gui` for `CLAP_EXT_GUI`.
        let mut g = unsafe { *(ext as *const clap_plugin_gui) };
        g.show = Some(gui_show);
        g.hide = Some(gui_hide);
        Gui(g)
    });
    &gui.0 as *const clap_plugin_gui as *const c_void
}

unsafe extern "C" fn get_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn get_plugin_descriptor(_factory: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
    if index == 0 { descriptor().clap_plugin_descriptor() } else { std::ptr::null() }
}

unsafe extern "C" fn create_plugin(_factory: *const clap_plugin_factory, host: *const clap_host, plugin_id: *const c_char) -> *const clap_plugin {
    // SAFETY: the host's NUL-terminated id.
    if plugin_id.is_null() || unsafe { CStr::from_ptr(plugin_id) } != descriptor().clap_id() {
        return std::ptr::null();
    }
    // As `nih_export_clap!`: the Arc is taken back in nih-plug's `destroy`.
    let wrapper = Arc::into_raw(Wrapper::<FfxSound>::new(host));
    // SAFETY: just made, and kept alive by the leaked Arc.
    let w = unsafe { &*wrapper };
    {
        let mut p = w.clap_plugin.borrow_mut();
        if let Some(f) = p.get_extension {
            NIH_GET_EXTENSION.get_or_init(|| f);
            p.get_extension = Some(get_extension);
        }
    }
    w.clap_plugin.as_ptr()
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(get_plugin_count),
    get_plugin_descriptor: Some(get_plugin_descriptor),
    create_plugin: Some(create_plugin),
};

extern "C" fn init(_plugin_path: *const c_char) -> bool {
    nih_plug::wrapper::setup_logger();
    true
}

extern "C" fn deinit() {}

extern "C" fn get_factory(factory_id: *const c_char) -> *const c_void {
    // SAFETY: the host's NUL-terminated id.
    if !factory_id.is_null() && unsafe { CStr::from_ptr(factory_id) } == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const clap_plugin_factory as *const c_void
    } else {
        std::ptr::null()
    }
}

/// The CLAP plugin's entry point.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
#[used]
pub static clap_entry: clap_plugin_entry =
    clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(init), deinit: Some(deinit), get_factory: Some(get_factory) };
