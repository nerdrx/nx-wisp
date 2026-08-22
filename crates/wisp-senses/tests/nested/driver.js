// Benchmark driver — the *opposite* of terrain.js, and it runs only inside the
// nested test compositor. It shoves a window around so there is something for
// the terrain feed to report. Never load this into a real session.
"use strict";

// These must be reachable from the top level. A QTimer referenced only by a
// scope that has returned is collected by KWin's JS engine and silently stops
// firing — which reads as "the feed produced one update and then died".
var nxbenchTimer = null;
var nxbenchTarget = null;
var nxbenchI = 0;

(function () {
    var list = workspace.windowList();
    for (var k = 0; k < list.length; k++) {
        if (list[k].normalWindow) {
            nxbenchTarget = list[k];
            break;
        }
    }
    if (!nxbenchTarget) {
        return;
    }
    nxbenchTimer = new QTimer();
    nxbenchTimer.interval = 1;
    nxbenchTimer.timeout.connect(function () {
        nxbenchI += 1;
        var g = nxbenchTarget.frameGeometry;
        nxbenchTarget.frameGeometry = {
            x: 100 + (nxbenchI % 600),
            y: 100 + ((nxbenchI * 3) % 400),
            width: g.width,
            height: g.height
        };
    });
    nxbenchTimer.start();
})();
