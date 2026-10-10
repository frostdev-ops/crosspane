// SPDX-License-Identifier: GPL-3.0-or-later
//
// Crosspane GNOME Shell extension (WP-G1.2, WP-G1.5 overlay half). Owner-approved 2026-10-09 as an
// isolated, opt-in module.
//
// It exports io.frostdev.Crosspane.Shell1 (io.frostdev.Crosspane.Shell1.xml next to this file is
// the one source of truth for the interface) on the session bus under io.frostdev.Crosspane.Shell
// so the Crosspane agent can list and place the user's windows and show its on-screen indicators.
//
// Rules this file keeps:
//  - Typed methods only. No eval, no Function(), no Shell.Eval, no property access by name from
//    the bus. Every argument is validated before anything happens; a bad argument is a D-Bus
//    error and has no effect.
//  - Eligible windows are Meta.Window toplevels of type NORMAL or DIALOG that are not
//    override-redirect and not skip-taskbar.
//  - Overlays never take input or focus and sit above everything, fullscreen windows included.
//  - Window titles and overlay text are never logged.
//  - disable() undoes everything: the bus name, the exported object, the timers, every signal
//    connection and every overlay actor.

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
const INTERFACE_VERSION = 1;

// WindowsChanged is coalesced: at most one per this many milliseconds.
const COALESCE_MS = 50;
// An overlay that is not painted within this long after ShowOverlay never reports visible.
const PAINT_TIMEOUT_MS = 1000;
const MAX_OVERLAYS = 16;
const MAX_TEXT_CHARS = 200;
const MAX_EXTENT = 32768;
const MAX_COORD = 1 << 20;
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

function appIdOf(tracker, window) {
    try {
        const id = tracker.get_window_app(window)?.get_id();
        if (id)
            return id;
    } catch {
        // Fall through to the window class.
    }
    return window.get_wm_class() || '';
}

function unmaximize(window) {
    if (!window.maximized_horizontally && !window.maximized_vertically)
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
