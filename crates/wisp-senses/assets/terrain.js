// NX Wisp — terrain feed.
//
// Runs inside KWin's script engine and streams two things to the wisp over
// D-Bus: which window has focus, and where every walkable window is. She stands
// on window edges (plan F4/F68), so geometry has to arrive while a drag is still
// happening, not after it.
//
// This script is read-only with respect to KWin. It never moves, resizes,
// activates or closes anything, and it never changes a setting.
//
// Placeholders below are substituted by wisp-senses when it writes the file:
//   __NX_WISP_SERVICE__  __NX_WISP_OBJECT__  __NX_WISP_IFACE__
//   __NX_WISP_EPOCH__    __NX_WISP_FLUSH_MS__
"use strict";

(function () {
    var SERVICE = "__NX_WISP_SERVICE__";
    var OBJECT = "__NX_WISP_OBJECT__";
    var IFACE = "__NX_WISP_IFACE__";
    var EPOCH = __NX_WISP_EPOCH__;
    var FLUSH_MS = __NX_WISP_FLUSH_MS__;
    var PROTOCOL = 1;

    // ---------------------------------------------------------------- state
    var registered = false;   // has the wisp answered Hello?
    var seq = 0;              // batch counter, for rate measurement
    var nextId = 1;           // dense u64 ids; Observation::Window wants a u64,
                              // KWin hands out QUuids.
    var idOf = {};            // uuid string -> u64
    var known = {};           // u64 -> last sent [x, y, w, h, gone]
    var dirty = {};           // u64 -> window object (or null once closed)
    var closedIds = [];       // u64 of windows that went away this batch
    var focusDirty = false;
    var flushQueued = false;

    function log(msg) {
        // KWin routes console.log to the journal under kwin_wayland.
        console.log("nx-wisp/terrain: " + msg);
    }

    // ------------------------------------------------------------- plumbing
    // KWin's script engine exposes QTimer. Without it we fall back to sending
    // each change immediately, which still works but costs one D-Bus call per
    // geometry step instead of one per frame.
    var haveTimer = (typeof QTimer !== "undefined");

    function makeTimer(ms, repeating, fn) {
        if (!haveTimer) {
            return null;
        }
        var t = new QTimer();
        t.interval = ms;
        t.singleShot = !repeating;
        t.timeout.connect(fn);
        return t;
    }

    var flushTimer = makeTimer(FLUSH_MS, false, function () {
        flushQueued = false;
        flush();
    });

    function scheduleFlush() {
        if (!flushTimer) {
            flush();
            return;
        }
        if (flushQueued) {
            return;
        }
        flushQueued = true;
        flushTimer.start();
    }

    // ------------------------------------------------------------- identity
    function uuidOf(w) {
        try {
            return String(w.internalId);
        } catch (e) {
            return null;
        }
    }

    function idFor(w) {
        var u = uuidOf(w);
        if (u === null) {
            return 0;
        }
        if (!idOf.hasOwnProperty(u)) {
            idOf[u] = nextId;
            nextId += 1;
        }
        return idOf[u];
    }

    function forget(w) {
        var u = uuidOf(w);
        if (u !== null && idOf.hasOwnProperty(u)) {
            var id = idOf[u];
            delete idOf[u];
            return id;
        }
        return 0;
    }

    // --------------------------------------------------------------- policy
    // What counts as terrain: things with a real, stable rectangle. Normal
    // toplevels and docks (she climbs the panel). Not menus, tooltips, splashes,
    // notifications or the desktop itself — those flicker in and out and would
    // be a floor that vanishes under her feet.
    function isTerrain(w) {
        if (!w) {
            return false;
        }
        try {
            if (w.deleted) {
                return false;
            }
            if (w.resourceClass === "nx-wisp" || w.resourceName === "nx-wisp") {
                return false; // never stand on herself
            }
            if (w.desktopWindow || w.splash || w.popupWindow || w.tooltip) {
                return false;
            }
            if (w.menu || w.dropdownMenu || w.notification || w.criticalNotification) {
                return false;
            }
            return w.normalWindow === true || w.dock === true || w.utility === true;
        } catch (e) {
            return false;
        }
    }

    // Terrain she can actually stand on right now.
    function isSolid(w) {
        try {
            if (!isTerrain(w)) {
                return false;
            }
            if (w.minimized === true || w.hidden === true) {
                return false;
            }
            if (w.onAllDesktops !== true && w.onCurrentDesktop === false) {
                return false;
            }
            var g = w.frameGeometry;
            return !!g && g.width > 0 && g.height > 0;
        } catch (e) {
            return false;
        }
    }

    function rectOf(w) {
        var g = w.frameGeometry;
        return [
            Math.round(g.x),
            Math.round(g.y),
            Math.round(g.width),
            Math.round(g.height)
        ];
    }

    // ---------------------------------------------------------------- focus
    function focusPayload() {
        var w = workspace.activeWindow;
        if (!w) {
            return ["", ""];
        }
        var app = "";
        var title = "";
        try {
            app = String(w.resourceClass || w.resourceName || "");
        } catch (e) {
            app = "";
        }
        try {
            title = String(w.caption || "");
        } catch (e) {
            title = "";
        }
        return [app, title];
    }

    // ---------------------------------------------------------------- flush
    function flush() {
        if (!registered) {
            return;
        }

        var wins = [];
        var id;

        for (id in dirty) {
            if (!dirty.hasOwnProperty(id)) {
                continue;
            }
            var w = dirty[id];
            var n = Number(id);
            if (!w || !isSolid(w)) {
                if (known.hasOwnProperty(id)) {
                    delete known[id];
                    wins.push([n, 0, 0, 0, 0, true]);
                }
                continue;
            }
            var r = rectOf(w);
            var prev = known[id];
            if (prev && prev[0] === r[0] && prev[1] === r[1] &&
                prev[2] === r[2] && prev[3] === r[3]) {
                continue; // nothing actually moved
            }
            known[id] = r;
            wins.push([n, r[0], r[1], r[2], r[3], false]);
        }
        dirty = {};

        while (closedIds.length > 0) {
            var cid = closedIds.pop();
            if (known.hasOwnProperty(cid)) {
                delete known[cid];
            }
            wins.push([cid, 0, 0, 0, 0, true]);
        }

        var focus = null;
        if (focusDirty) {
            focusDirty = false;
            focus = focusPayload();
        }

        if (wins.length === 0 && focus === null) {
            return;
        }

        seq += 1;
        var payload = {
            v: PROTOCOL,
            e: EPOCH,
            s: seq,
            t: Date.now(),
            w: wins
        };
        if (focus !== null) {
            payload.f = focus;
        }

        callDBus(SERVICE, OBJECT, IFACE, "Batch", JSON.stringify(payload));
    }

    // ------------------------------------------------------------ tracking
    function touch(w) {
        var id = idFor(w);
        if (id !== 0) {
            dirty[id] = w;
            scheduleFlush();
        }
    }

    function connectIf(obj, name, fn) {
        try {
            if (obj[name] && typeof obj[name].connect === "function") {
                obj[name].connect(fn);
                return true;
            }
        } catch (e) {
            // Signal absent on this KWin version; the poll-free path just loses
            // one trigger, it does not break the feed.
        }
        return false;
    }

    function track(w) {
        if (!isTerrain(w)) {
            return;
        }
        connectIf(w, "frameGeometryChanged", function () { touch(w); });
        connectIf(w, "interactiveMoveResizeStepped", function () { touch(w); });
        connectIf(w, "minimizedChanged", function () { touch(w); });
        connectIf(w, "desktopsChanged", function () { touch(w); });
        connectIf(w, "outputChanged", function () { touch(w); });
        connectIf(w, "maximizedChanged", function () { touch(w); });
        connectIf(w, "fullScreenChanged", function () { touch(w); });
        connectIf(w, "captionChanged", function () {
            if (workspace.activeWindow === w) {
                focusDirty = true;
                scheduleFlush();
            }
        });
        connectIf(w, "closed", function () {
            var id = forget(w);
            if (id !== 0) {
                delete dirty[id];
                closedIds.push(id);
                scheduleFlush();
            }
        });
        touch(w);
    }

    // A full restatement of the world. Sent after Hello, and after anything
    // that could have invalidated our view (desktop switch).
    function resync() {
        var list = workspace.windowList();
        for (var i = 0; i < list.length; i++) {
            if (isTerrain(list[i])) {
                dirty[idFor(list[i])] = list[i];
            }
        }
        // Anything we believed in but that is no longer listed is gone.
        for (var id in known) {
            if (known.hasOwnProperty(id) && !dirty.hasOwnProperty(id)) {
                dirty[id] = null;
            }
        }
        focusDirty = true;
        scheduleFlush();
    }

    // --------------------------------------------------------- registration
    function sayHello() {
        if (registered) {
            return;
        }
        // Everything crosses the bus as one JSON string. KWin's callDBus infers
        // the D-Bus signature from the JS value's type, and a number that does
        // not fit an int arrives as a double — which would not match a fixed
        // signature and the call would simply never land. Strings are exact.
        var hello = JSON.stringify({ v: PROTOCOL, e: EPOCH });
        callDBus(SERVICE, OBJECT, IFACE, "Hello", hello, function (ok) {
            if (registered) {
                return;
            }
            registered = true;
            if (helloTimer) {
                helloTimer.stop();
            }
            log("registered with the wisp, epoch " + EPOCH);
            resync();
        });
    }

    var helloTimer = makeTimer(1000, true, sayHello);

    // ---------------------------------------------------------------- start
    var initial = workspace.windowList();
    for (var i = 0; i < initial.length; i++) {
        track(initial[i]);
    }

    connectIf(workspace, "windowAdded", track);
    connectIf(workspace, "windowRemoved", function (w) {
        var id = forget(w);
        if (id !== 0) {
            delete dirty[id];
            closedIds.push(id);
            scheduleFlush();
        }
    });
    connectIf(workspace, "windowActivated", function () {
        focusDirty = true;
        scheduleFlush();
    });
    connectIf(workspace, "currentDesktopChanged", resync);

    sayHello();
    if (helloTimer) {
        helloTimer.start();
    }
    log("loaded, protocol " + PROTOCOL + ", flush " + FLUSH_MS +
        "ms, timers " + (haveTimer ? "yes" : "no"));
})();
