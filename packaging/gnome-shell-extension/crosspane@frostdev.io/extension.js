// SPDX-License-Identifier: GPL-3.0-or-later
//
// Crosspane GNOME Shell extension (WP-G1.2, WP-G1.5 overlay half; v2 WP-G2.4 task C). Owner-approved
// 2026-10-09 as an isolated, opt-in module; the v2 methods (pointer fence, layout snapshot, cursor
// hiding) were approved on 2026-10-10 with the virtual twin monitor.
//
// It exports io.frostdev.Crosspane.Shell1 (io.frostdev.Crosspane.Shell1.xml next to this file is
// the one source of truth for the interface) on the session bus under io.frostdev.Crosspane.Shell
// so the Crosspane agent can list and place the user's windows, show its on-screen indicators,
// fence the local pointer out of its virtual twin monitor, snapshot and put back the window
// layout around a monitor change, and hide the local pointer while it captures input.
//
// Rules this file keeps:
//  - Typed methods only. No eval, no Function(), no Shell.Eval, no property access by name from
//    the bus. Every argument is validated before anything happens; a bad argument is a D-Bus
//    error and has no effect.
//  - Eligible windows are Meta.Window toplevels of type NORMAL or DIALOG that are not
//    override-redirect and not skip-taskbar.
//  - Overlays never take input or focus and sit above everything, fullscreen windows included.
//  - Window titles and overlay text are never logged.
//  - The pointer fence and the hidden cursor belong to the D-Bus connection that set them: when
//    that connection goes away (the agent crashed) the fence is removed and the cursor shown
//    again, so a dead agent can never leave the user without a pointer.
//  - disable() undoes everything: the bus name, the exported object, the timers, every signal
//    connection, every overlay actor, the fence, the layout snapshots and the cursor inhibition.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Mtk from 'gi://Mtk';
import Pango from 'gi://Pango';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS_NAME = 'io.frostdev.Crosspane.Shell';
const OBJECT_PATH = '/io/frostdev/Crosspane/Shell';
const INTERFACE_XML_FILE = 'io.frostdev.Crosspane.Shell1.xml';
const INTERFACE_VERSION = 2;

// WindowsChanged is coalesced: at most one per this many milliseconds.
const COALESCE_MS = 50;
// An overlay that is not painted within this long after ShowOverlay never reports visible.
const PAINT_TIMEOUT_MS = 1000;
const MAX_OVERLAYS = 16;
// SaveLayout keeps this many snapshots; a further one drops the oldest.
const MAX_SNAPSHOTS = 4;
// The most window ids one RestoreLayout call may skip.
const MAX_SKIP_IDS = 4096;
const MAX_TEXT_CHARS = 200;
const MAX_EXTENT = 32768;
const MAX_COORD = 1 << 20;
// Meta.Barrier coordinates are 0..G_MAXSHORT.
const MAX_BARRIER_COORD = 32767;
const MAX_UINT32 = 0xffffffff;
const MAX_RGB = 0xffffff;
const OVERLAY_MARGIN = 16;
// All overlay styling is inline: nothing depends on a stylesheet being loaded.
const OVERLAY_BOX_STYLE = 'border-radius: 10px; padding: 6px 14px; ' +
    'border: 2px solid rgba(255, 255, 255, 0.75);';
const OVERLAY_LABEL_STYLE = 'font-weight: bold; font-size: 11pt;';

// Overlay anchors, as in the interface.
const ANCHOR_TOP_CENTER = 0;
const ANCHOR_TOP_RIGHT = 1;
const ANCHOR_BOTTOM_RIGHT = 2;
const ANCHOR_CENTER = 3;

// Per-window signals that can change what ListWindows reports.
const WINDOW_SIGNALS = [
    'notify::title',
    'position-changed',
    'size-changed',
    'notify::minimized',
    'notify::fullscreen',
    'notify::skip-taskbar',
    'notify::window-type',
];

/**
 * A D-Bus error for a bad argument. wrapJSObject sends a GLib.Error thrown by a method as the
 * matching D-Bus error reply.
 *
 * @param {number} code a Gio.DBusError code
 * @param {string} message what was wrong; never a window title or overlay text
 * @returns {GLib.Error} the error to throw
 */
function dbusError(code, message) {
    return GLib.Error.new_literal(Gio.DBusError, code, message);
}

function invalidArgs(message) {
    return dbusError(Gio.DBusError.INVALID_ARGS, message);
}

function checkInt(name, value, min, max) {
    if (!Number.isInteger(value) || value < min || value > max)
        throw invalidArgs(`${name} is out of range`);
}

function checkBool(name, value) {
    if (typeof value !== 'boolean')
        throw invalidArgs(`${name} must be a boolean`);
}

/**
 * Window ids arrive as 't' values; GJS unpacks them as numbers (or BigInt on some versions).
 *
 * @param {number|bigint} value the unpacked id
 * @returns {number} the id as a number, or NaN if it is not a plain unsigned integer
 */
function windowKey(value) {
    const key = typeof value === 'bigint' ? Number(value) : value;
    return Number.isSafeInteger(key) && key >= 0 ? key : NaN;
}

/**
 * Random per enable(): 53 bits from two 32-bit draws, so the value is exact as a JS number and
 * as a D-Bus 't'. Never 0.
 *
 * @returns {number} the epoch
 */
function randomEpoch() {
    const high = GLib.random_int() & 0x1fffff;
    const low = GLib.random_int();
    const epoch = high * 4294967296 + low;
    return epoch === 0 ? 1 : epoch;
}

/**
 * Eligible windows: NORMAL or DIALOG toplevels that are neither override-redirect nor
 * skip-taskbar.
 *
 * @param {Meta.Window} window the window
 * @returns {boolean} whether the bridge exposes it
 */
function isEligible(window) {
    try {
        const type = window.get_window_type();
        return (type === Meta.WindowType.NORMAL || type === Meta.WindowType.DIALOG) &&
            !window.is_override_redirect() &&
            !window.is_skip_taskbar();
    } catch {
        // A window that is being torn down can throw on access; it is not eligible any more.
        return false;
    }
}

/**
 * The app id ListWindows reports: the Shell's id of the matching application (for example
 * "org.gnome.Nautilus.desktop"), else the window class, else "". The Shell invents "window:N" ids
 * for windows no application matches; those are never reported.
 *
 * @param {Shell.WindowTracker} tracker the window tracker
 * @param {Meta.Window} window the window
 * @returns {string} the app id
 */
function appIdOf(tracker, window) {
    try {
        const app = tracker.get_window_app(window);
        const id = app && !app.is_window_backed() ? app.get_id() : '';
        if (id && !id.startsWith('window:'))
            return id;
    } catch {
        // Fall through to the window class.
    }
    try {
        return window.get_wm_class() || '';
    } catch {
        return '';
    }
}

/**
 * The maximize flags of a window (Meta.MaximizeFlags bits; 0 when not maximized).
 *
 * @param {Meta.Window} window the window
 * @returns {number} the flags
 */
function maximizeFlagsOf(window) {
    // Mutter 49 and later: get_maximize_flags(). Mutter 48: get_maximized().
    if (typeof window.get_maximize_flags === 'function')
        return window.get_maximize_flags();
    return window.get_maximized();
}

/**
 * Maximize a window in the given directions. Mutter asserts on empty flags, so 0 does nothing.
 *
 * @param {Meta.Window} window the window
 * @param {number} flags Meta.MaximizeFlags bits
 */
function applyMaximizeFlags(window, flags) {
    if (flags === 0)
        return;
    // Mutter 49 and later: set_maximize_flags(). Mutter 48: maximize(directions).
    if (typeof window.set_maximize_flags === 'function')
        window.set_maximize_flags(flags);
    else
        window.maximize(flags);
}

function unmaximize(window) {
    if (maximizeFlagsOf(window) === 0)
        return;
    try {
        // Mutter 49 and later: no flags.
        window.unmaximize();
    } catch {
        // Mutter 48: the directions to unmaximize.
        window.unmaximize(Meta.MaximizeFlags.BOTH);
    }
}

/**
 * The four barriers of a pointer fence around one rectangle. Each lets the pointer pass only
 * outward (Meta.Barrier `directions` are the directions of motion that pass), so the pointer
 * cannot enter the rectangle but can leave it.
 *
 * A pointer stopped by a barrier rests exactly on the barrier's line, and the pixel under a
 * pointer at position p is floor(p). The rectangle covers the pixels x..x+width-1, so the right
 * and bottom lines (x+width, y+height) already rest the pointer on the first pixel outside it. The
 * left and top lines are one pixel further out (x-1, y-1): on x itself the pointer would rest on
 * the rectangle's own first column and hover whatever window is there.
 *
 * Meta.Barrier takes coordinates 0..MAX_BARRIER_COORD only (a value outside that is replaced by
 * the property's default, silently misplacing the barrier), so the caller checks the rectangle
 * against that range; the left and top lines stop at 0, past which there is no screen anyway.
 */
class PointerFence {
    constructor(x, y, width, height) {
        const left = Math.max(0, x - 1);
        const top = Math.max(0, y - 1);
        const right = x + width;
        const bottom = y + height;
        const {NEGATIVE_X, NEGATIVE_Y, POSITIVE_X, POSITIVE_Y} = Meta.BarrierDirection;
        const edges = [
            [left, top, left, bottom, NEGATIVE_X],
            [left, top, right, top, NEGATIVE_Y],
            [right, top, right, bottom, POSITIVE_X],
            [left, bottom, right, bottom, POSITIVE_Y],
        ];
        this._barriers = [];
        try {
            for (const [x1, y1, x2, y2, directions] of edges) {
                this._barriers.push(new Meta.Barrier({
                    backend: global.backend,
                    x1, y1, x2, y2, directions,
                }));
            }
        } catch (error) {
            this.destroy();
            throw error;
        }
    }

    destroy() {
        for (const barrier of this._barriers) {
            try {
                barrier.destroy();
            } catch {
                // Already gone.
            }
        }
        this._barriers = [];
    }
}

/**
 * Calls `onVanished` once when the D-Bus connection `name` (a unique name) leaves the bus.
 */
class SenderWatch {
    constructor(name, onVanished) {
        this.name = name;
        this._id = Gio.bus_watch_name_on_connection(
            Gio.DBus.session, name, Gio.BusNameWatcherFlags.NONE, null, () => onVanished());
    }

    destroy() {
        if (this._id) {
            Gio.bus_unwatch_name(this._id);
            this._id = 0;
        }
    }
}

/**
 * Plain text for an overlay: control characters become spaces and the length is capped. St.Label
 * does not interpret markup, so nothing else needs escaping.
 *
 * @param {string} text the requested text
 * @returns {string} the text to display
 */
function sanitizeText(text) {
    const chars = Array.from(String(text).replace(/[\u0000-\u001f\u007f]/g, ' '));
    if (chars.length <= MAX_TEXT_CHARS)
        return chars.join('');
    return `${chars.slice(0, MAX_TEXT_CHARS - 1).join('')}…`;
}

/**
 * The index of the monitor that contains the logical point (x, y), or -1.
 *
 * @param {number} x logical x
 * @param {number} y logical y
 * @returns {number} the Shell's monitor index
 */
function monitorIndexAt(x, y) {
    const rect = new Mtk.Rectangle({x, y, width: 1, height: 1});
    const index = global.display.get_monitor_index_for_rect(rect);
    const monitor = Main.layoutManager.monitors[index];
    // get_monitor_index_for_rect may answer with the closest monitor; the point must be inside.
    if (index < 0 || !monitor ||
        x < monitor.x || y < monitor.y ||
        x >= monitor.x + monitor.width || y >= monitor.y + monitor.height)
        return -1;
    return index;
}

/**
 * One overlay: a label in a box in the top layer of the UI group. Never reactive, so it cannot
 * take input or focus.
 */
class Overlay {
    constructor(bridge, id) {
        this._bridge = bridge;
        this.id = id;
        this._x = 0;
        this._y = 0;
        this._anchor = ANCHOR_TOP_CENTER;
        this._monitor = -1;
        this._placed = null;
        this._allocationId = 0;
        this._paintId = 0;
        this._paintTimeoutId = 0;

        this._boxStyle = '';
        this._appliedStyle = null;
        this._label = new St.Label({
            reactive: false,
            can_focus: false,
            track_hover: false,
        });
        this._label.clutter_text.ellipsize = Pango.EllipsizeMode.END;
        this._actor = new St.BoxLayout({
            reactive: false,
            can_focus: false,
            track_hover: false,
            visible: false,
        });
        this._actor.add_child(this._label);
        // Added last, so above the window groups and every chrome actor, fullscreen windows
        // included. Plain add_child, not layoutManager.addChrome: the overlay must not affect
        // struts, the input region or fullscreen tracking.
        Main.uiGroup.add_child(this._actor);
        this._allocationId = this._actor.connect('notify::allocation', () => this._place());
    }

    /**
     * Apply a ShowOverlay request. The monitor index was resolved by the caller.
     */
    update(x, y, anchor, text, accent, monitor) {
        this._x = x;
        this._y = y;
        this._anchor = anchor;
        this._monitor = monitor;
        this._label.text = sanitizeText(text);

        const red = (accent >> 16) & 0xff;
        const green = (accent >> 8) & 0xff;
        const blue = accent & 0xff;
        const luminance = (0.2126 * red + 0.7152 * green + 0.0722 * blue) / 255;
        const textColor = luminance > 0.6 ? '#111111' : '#ffffff';
        this._label.set_style(`${OVERLAY_LABEL_STYLE} color: ${textColor};`);
        this._boxStyle = `${OVERLAY_BOX_STYLE} background-color: rgba(${red}, ${green}, ${blue}, 0.92);`;

        this._placed = null;
        this._place();
        this._actor.show();
        this._armPaintReport();
    }

    /**
     * Put the overlay back on the monitor under its requested point after the monitors changed.
     *
     * @returns {boolean} false if no monitor contains the point any more
     */
    relocate() {
        const monitor = monitorIndexAt(this._x, this._y);
        if (monitor < 0)
            return false;
        this._monitor = monitor;
        this._place();
        return true;
    }

    destroy() {
        this._cancelPaintReport();
        if (this._allocationId) {
            this._actor.disconnect(this._allocationId);
            this._allocationId = 0;
        }
        // Destroying the actor removes it from the UI group and destroys the label.
        this._actor.destroy();
    }

    _place() {
        const monitor = Main.layoutManager.monitors[this._monitor];
        if (!monitor)
            return;

        // Width limit first: the natural size depends on it.
        const maxWidth = Math.max(100, Math.floor(monitor.width * 0.9));
        const style = `${this._boxStyle} max-width: ${maxWidth}px;`;
        if (style !== this._appliedStyle) {
            this._appliedStyle = style;
            this._actor.set_style(style);
        }
        const [, , width, height] = this._actor.get_preferred_size();

        // Keep clear of the top bar on the primary monitor.
        const panelHeight = (this._monitor === Main.layoutManager.primaryIndex && Main.panel)
            ? Main.panel.height : 0;
        let x;
        let y;
        switch (this._anchor) {
        case ANCHOR_TOP_RIGHT:
            x = monitor.x + monitor.width - width - OVERLAY_MARGIN;
            y = monitor.y + panelHeight + OVERLAY_MARGIN;
            break;
        case ANCHOR_BOTTOM_RIGHT:
            x = monitor.x + monitor.width - width - OVERLAY_MARGIN;
            y = monitor.y + monitor.height - height - OVERLAY_MARGIN;
            break;
        case ANCHOR_CENTER:
            x = monitor.x + (monitor.width - width) / 2;
            y = monitor.y + (monitor.height - height) / 2;
            break;
        case ANCHOR_TOP_CENTER:
        default:
            x = monitor.x + (monitor.width - width) / 2;
            y = monitor.y + panelHeight + OVERLAY_MARGIN;
            break;
        }
        x = Math.round(x);
        y = Math.round(y);

        // notify::allocation also fires when only the position changed: skip identical results.
        const key = `${x},${y},${width},${height}`;
        if (key === this._placed)
            return;
        this._placed = key;
        this._actor.set_position(x, y);
    }

    /**
     * Report OverlayState(id, true) after the next stage paint in which the actor is mapped.
     */
    _armPaintReport() {
        this._cancelPaintReport();
        this._paintId = global.stage.connect('after-paint', () => {
            if (!this._actor.mapped || !this._actor.visible)
                return;
            this._cancelPaintReport();
            this._bridge.emitOverlayState(this.id, true);
        });
        this._paintTimeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, PAINT_TIMEOUT_MS, () => {
            this._paintTimeoutId = 0;
            this._cancelPaintReport();
            console.debug('Crosspane: an overlay was not painted in time');
            return GLib.SOURCE_REMOVE;
        });
        this._actor.queue_redraw();
    }

    _cancelPaintReport() {
        if (this._paintId) {
            global.stage.disconnect(this._paintId);
            this._paintId = 0;
        }
        if (this._paintTimeoutId) {
            GLib.Source.remove(this._paintTimeoutId);
            this._paintTimeoutId = 0;
        }
    }
}

/**
 * The exported object. Method and property names are the interface's; wrapJSObject calls them
 * with the unpacked arguments and packs the return value by the interface's signatures.
 */
class Bridge {
    constructor(interfaceXml) {
        this._interfaceXml = interfaceXml;
        this._epoch = randomEpoch();
        this._tracker = Shell.WindowTracker.get_default();
        this._dbus = null;
        this._ownerId = 0;
        this._connections = [];
        // Meta.Window id -> {window, ids, eligible}.
        this._windows = new Map();
        this._overlays = new Map();
        this._changedSource = 0;
        this._stopped = false;
        // v2: the pointer fence, the layout snapshots and the cursor inhibition.
        this._fence = null;
        this._fenceWatch = null;
        // token -> [{key, x, y, width, height, flags, fullscreen}], oldest first.
        this._snapshots = new Map();
        this._nextToken = 1;
        this._cursorInhibited = false;
        this._cursorWatch = null;
    }

    // Properties.

    get Version() {
        return INTERFACE_VERSION;
    }

    get ShellEpoch() {
        return this._epoch;
    }

    // Lifecycle.

    start() {
        const display = global.display;
        this._connect(display, 'window-created', (_display, window) => this._trackWindow(window));
        this._connect(display, 'notify::focus-window', () => this._queueChanged());
        this._connect(Main.layoutManager, 'monitors-changed', () => this._monitorsChanged());
        for (const window of display.list_all_windows())
            this._trackWindow(window);

        // Export before owning the name, so the object answers the moment the name is acquired.
        this._dbus = Gio.DBusExportedObject.wrapJSObject(this._interfaceXml, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._ownerId = Gio.bus_own_name_on_connection(
            Gio.DBus.session, BUS_NAME, Gio.BusNameOwnerFlags.NONE,
            () => console.debug('Crosspane: bus name acquired'),
            () => console.debug('Crosspane: bus name not available'));
    }

    stop() {
        if (this._stopped)
            return;
        this._stopped = true;

        if (this._changedSource) {
            GLib.Source.remove(this._changedSource);
            this._changedSource = 0;
        }

        // Overlays first, while the object is still exported, so clients see them go.
        for (const id of [...this._overlays.keys()])
            this._dropOverlay(id, true);

        // The pointer is free, the cursor is shown and no snapshot outlives the extension.
        this._clearFence();
        try {
            this._setCursorInhibited(false, null);
        } catch (error) {
            console.debug(`Crosspane: could not show the cursor again: ${error.message}`);
        }
        this._snapshots.clear();

        for (const [object, id] of this._connections) {
            try {
                object.disconnect(id);
            } catch {
                // The object is already gone.
            }
        }
        this._connections = [];
        for (const entry of [...this._windows.values()])
            this._untrackWindow(entry);

        if (this._dbus) {
            this._dbus.unexport();
            this._dbus = null;
        }
        if (this._ownerId) {
            Gio.bus_unown_name(this._ownerId);
            this._ownerId = 0;
        }
    }

    _connect(object, signal, handler) {
        this._connections.push([object, object.connect(signal, handler)]);
    }

    // Window tracking.

    _trackWindow(window) {
        const key = window.get_id();
        if (this._windows.has(key))
            return;
        const entry = {key, window, ids: [], eligible: isEligible(window)};
        const touched = () => this._windowTouched(entry);
        for (const signal of WINDOW_SIGNALS)
            entry.ids.push(window.connect(signal, touched));
        entry.ids.push(window.connect('unmanaged', () => this._windowUnmanaged(key)));
        this._windows.set(key, entry);
        if (entry.eligible)
            this._queueChanged();
    }

    _untrackWindow(entry) {
        for (const id of entry.ids) {
            try {
                entry.window.disconnect(id);
            } catch {
                // The window is already gone.
            }
        }
        entry.ids = [];
        this._windows.delete(entry.key);
    }

    _windowTouched(entry) {
        const was = entry.eligible;
        entry.eligible = isEligible(entry.window);
        if (entry.eligible || was)
            this._queueChanged();
    }

    _windowUnmanaged(key) {
        const entry = this._windows.get(key);
        if (!entry)
            return;
        this._untrackWindow(entry);
        if (entry.eligible)
            this._queueChanged();
    }

    _queueChanged() {
        if (this._changedSource || this._stopped)
            return;
        this._changedSource = GLib.timeout_add(GLib.PRIORITY_DEFAULT, COALESCE_MS, () => {
            this._changedSource = 0;
            this._emit('WindowsChanged', new GLib.Variant('(t)', [this._epoch]));
            return GLib.SOURCE_REMOVE;
        });
    }

    _emit(name, parameters) {
        if (!this._dbus)
            return;
        try {
            this._dbus.emit_signal(name, parameters);
        } catch (error) {
            console.debug(`Crosspane: could not emit ${name}: ${error.message}`);
        }
    }

    emitOverlayState(id, visible) {
        this._emit('OverlayState', new GLib.Variant('(ub)', [id, visible]));
    }

    _find(id) {
        const entry = this._windows.get(windowKey(id));
        if (!entry || !isEligible(entry.window))
            return null;
        return entry.window;
    }

    // Methods.

    ListWindows() {
        const windows = [];
        for (const [id, entry] of this._windows) {
            const window = entry.window;
            if (!isEligible(window))
                continue;
            const rect = window.get_frame_rect();
            windows.push([
                id,
                appIdOf(this._tracker, window),
                window.get_title() ?? '',
                Math.max(0, window.get_pid() | 0),
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                window.has_focus(),
                window.minimized,
                window.is_fullscreen(),
            ]);
        }
        windows.sort((a, b) => a[0] - b[0]);
        return [this._epoch, windows];
    }

    Activate(id) {
        const window = this._find(id);
        if (!window)
            return false;
        Main.activateWindow(window);
        return true;
    }

    MoveResize(id, x, y, width, height) {
        checkInt('x', x, -MAX_COORD, MAX_COORD);
        checkInt('y', y, -MAX_COORD, MAX_COORD);
        checkInt('width', width, 1, MAX_EXTENT);
        checkInt('height', height, 1, MAX_EXTENT);
        const window = this._find(id);
        if (!window)
            return false;
        unmaximize(window);
        if (window.is_fullscreen())
            window.unmake_fullscreen();
        window.move_resize_frame(true, x, y, width, height);
        return true;
    }

    SetMinimized(id, minimized) {
        checkBool('minimized', minimized);
        const window = this._find(id);
        if (!window)
            return false;
        if (minimized) {
            if (!window.can_minimize())
                throw dbusError(Gio.DBusError.NOT_SUPPORTED, 'the window cannot be minimized');
            window.minimize();
        } else {
            window.unminimize();
        }
        return true;
    }

    Close(id) {
        const window = this._find(id);
        if (!window)
            return false;
        window.delete(global.get_current_time());
        return true;
    }

    ShowOverlay(id, x, y, anchor, text, accent) {
        checkInt('id', id, 0, MAX_UINT32);
        checkInt('x', x, -MAX_COORD, MAX_COORD);
        checkInt('y', y, -MAX_COORD, MAX_COORD);
        checkInt('anchor', anchor, ANCHOR_TOP_CENTER, ANCHOR_CENTER);
        checkInt('accent', accent, 0, MAX_RGB);
        if (typeof text !== 'string')
            throw invalidArgs('text must be a string');

        const monitor = monitorIndexAt(x, y);
        if (monitor < 0)
            throw invalidArgs('the point is not on any monitor');

        let overlay = this._overlays.get(id);
        if (!overlay) {
            if (this._overlays.size >= MAX_OVERLAYS)
                throw dbusError(Gio.DBusError.LIMITS_EXCEEDED, 'too many overlays');
            overlay = new Overlay(this, id);
            this._overlays.set(id, overlay);
        }
        overlay.update(x, y, anchor, text, accent, monitor);
    }

    HideOverlay(id) {
        checkInt('id', id, 0, MAX_UINT32);
        // Hiding an overlay that is not shown succeeds and emits nothing.
        this._dropOverlay(id, true);
    }

    _dropOverlay(id, report) {
        const overlay = this._overlays.get(id);
        if (!overlay)
            return;
        this._overlays.delete(id);
        overlay.destroy();
        if (report)
            this.emitOverlayState(id, false);
    }

    // v2: pointer fence, layout snapshot, cursor hiding.

    // SetPointerFence and InhibitCursor are written in GJS's "<Name>Async" form: they receive the
    // method invocation, which names the calling D-Bus connection. They answer through _answer.

    SetPointerFenceAsync([x, y, width, height], invocation) {
        this._answer(invocation, () =>
            this._setPointerFence(x, y, width, height, invocation.get_sender()));
    }

    ClearPointerFence() {
        this._clearFence();
    }

    SaveLayout() {
        if (this._nextToken > MAX_UINT32)
            throw dbusError(Gio.DBusError.LIMITS_EXCEEDED, 'out of layout tokens');
        const windows = [];
        for (const [key, entry] of this._windows) {
            const window = entry.window;
            if (!isEligible(window))
                continue;
            const rect = window.get_frame_rect();
            windows.push({
                key,
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                flags: maximizeFlagsOf(window),
                fullscreen: window.is_fullscreen(),
            });
        }
        // At most MAX_SNAPSHOTS are kept; a new one drops the oldest. Tokens count up from 1 and
        // are never reused within this epoch.
        while (this._snapshots.size >= MAX_SNAPSHOTS)
            this._snapshots.delete(this._snapshots.keys().next().value);
        const token = this._nextToken++;
        this._snapshots.set(token, windows);
        return token;
    }

    RestoreLayout(token, skip) {
        checkInt('token', token, 1, MAX_UINT32);
        if (!Array.isArray(skip) || skip.length > MAX_SKIP_IDS)
            throw invalidArgs('skip must be an array of at most 4096 window ids');
        const skipped = new Set();
        for (const id of skip) {
            const key = windowKey(id);
            if (Number.isNaN(key))
                throw invalidArgs('skip holds an invalid window id');
            skipped.add(key);
        }
        const snapshot = this._snapshots.get(token);
        if (!snapshot)
            throw invalidArgs('unknown layout token');
        // The snapshot is used up whatever happens to the windows.
        this._snapshots.delete(token);

        let restored = 0;
        for (const saved of snapshot) {
            if (skipped.has(saved.key))
                continue;
            const entry = this._windows.get(saved.key);
            if (!entry || !isEligible(entry.window))
                continue;
            try {
                if (this._restoreWindow(entry.window, saved))
                    restored++;
            } catch (error) {
                console.debug(`Crosspane: could not restore a window: ${error.message}`);
            }
        }
        return restored;
    }

    InhibitCursorAsync([inhibit], invocation) {
        this._answer(invocation, () => {
            checkBool('inhibit', inhibit);
            this._setCursorInhibited(inhibit, invocation.get_sender());
        });
    }

    /**
     * Answer a method written in the "<Name>Async" form: an empty reply, or the D-Bus error for a
     * GLib.Error thrown by `action` (bad arguments). Anything else is an internal error whose
     * text is not sent back.
     */
    _answer(invocation, action) {
        try {
            action();
        } catch (error) {
            if (error instanceof GLib.Error) {
                invocation.return_gerror(error);
            } else {
                console.debug(`Crosspane: a method failed: ${error?.message}`);
                invocation.return_dbus_error('org.freedesktop.DBus.Error.Failed', 'internal error');
            }
            return;
        }
        invocation.return_value(null);
    }

    _setPointerFence(x, y, width, height, sender) {
        checkInt('x', x, -MAX_COORD, MAX_COORD);
        checkInt('y', y, -MAX_COORD, MAX_COORD);
        checkInt('width', width, 1, MAX_EXTENT);
        checkInt('height', height, 1, MAX_EXTENT);
        // Mutter's layouts never reach negative coordinates and Meta.Barrier takes 0..32767 only:
        // a rectangle outside that cannot be fenced, and must not be fenced wrongly.
        if (x < 0 || y < 0 || x + width > MAX_BARRIER_COORD || y + height > MAX_BARRIER_COORD)
            throw invalidArgs('the fence must lie within 0..32767 on both axes');
        // The old fence goes first, so a failure below leaves the pointer free rather than fenced
        // by something stale.
        this._clearFence();
        try {
            this._fence = new PointerFence(x, y, width, height);
        } catch (error) {
            console.debug(`Crosspane: could not create the pointer fence: ${error.message}`);
            throw dbusError(Gio.DBusError.FAILED, 'the pointer fence could not be created');
        }
        // The fence belongs to its caller: it goes when that connection does.
        if (sender)
            this._fenceWatch = new SenderWatch(sender, () => this._clearFence());
    }

    _clearFence() {
        this._fenceWatch?.destroy();
        this._fenceWatch = null;
        this._fence?.destroy();
        this._fence = null;
    }

    /**
     * Hide or show the local pointer. Idempotent per state: Mutter counts inhibitions, so a second
     * `true` must not add another one. `sender` is the calling connection, which owns the
     * inhibition until it asks for the cursor back or leaves the bus.
     */
    _setCursorInhibited(inhibit, sender) {
        if (inhibit) {
            if (!this._cursorInhibited) {
                global.backend.get_cursor_tracker().inhibit_cursor_visibility();
                this._cursorInhibited = true;
            }
            if (sender && this._cursorWatch?.name !== sender) {
                this._cursorWatch?.destroy();
                this._cursorWatch =
                    new SenderWatch(sender, () => this._setCursorInhibited(false, null));
            }
            return;
        }
        this._cursorWatch?.destroy();
        this._cursorWatch = null;
        if (this._cursorInhibited) {
            this._cursorInhibited = false;
            global.backend.get_cursor_tracker().uninhibit_cursor_visibility();
        }
    }

    /**
     * Put one window back as the snapshot had it, in this order: fullscreen again if it was;
     * maximized with the saved flags if it was; otherwise unfullscreen/unmaximize if it changed
     * and move_resize_frame(false, saved rect). Tiling cannot be restored through public API: a
     * tiled window gets its tiled rect back as a floating window.
     *
     * @returns {boolean} whether anything was changed
     */
    _restoreWindow(window, saved) {
        const fullscreen = window.is_fullscreen();
        const flags = maximizeFlagsOf(window);
        let changed = false;
        if (saved.fullscreen) {
            if (!fullscreen) {
                window.make_fullscreen();
                changed = true;
            }
        } else if (saved.flags !== 0) {
            if (fullscreen) {
                window.unmake_fullscreen();
                changed = true;
            }
            if (flags !== saved.flags) {
                // set_maximize_flags only adds directions, so drop extra ones first.
                if ((flags & ~saved.flags) !== 0)
                    unmaximize(window);
                applyMaximizeFlags(window, saved.flags);
                changed = true;
            }
        } else {
            if (fullscreen) {
                window.unmake_fullscreen();
                changed = true;
            }
            if (flags !== 0) {
                unmaximize(window);
                changed = true;
            }
            const rect = window.get_frame_rect();
            if (rect.x !== saved.x || rect.y !== saved.y ||
                rect.width !== saved.width || rect.height !== saved.height) {
                window.move_resize_frame(false, saved.x, saved.y, saved.width, saved.height);
                changed = true;
            }
        }
        return changed;
    }

    _monitorsChanged() {
        for (const [id, overlay] of [...this._overlays]) {
            // An overlay whose point is on no monitor any more cannot be kept on screen.
            if (!overlay.relocate())
                this._dropOverlay(id, true);
        }
    }
}

function loadInterfaceXml(dir) {
    const [, bytes] = dir.get_child(INTERFACE_XML_FILE).load_contents(null);
    return new TextDecoder().decode(bytes);
}

export default class CrosspaneExtension extends Extension {
    enable() {
        const bridge = new Bridge(loadInterfaceXml(this.dir));
        try {
            bridge.start();
        } catch (error) {
            bridge.stop();
            throw error;
        }
        this._bridge = bridge;
        console.debug('Crosspane: enabled');
    }

    disable() {
        this._bridge?.stop();
        this._bridge = null;
        console.debug('Crosspane: disabled');
    }
}
