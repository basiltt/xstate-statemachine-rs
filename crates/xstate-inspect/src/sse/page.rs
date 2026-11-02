//! The static page and script served by [`super::SseSink`] (ported verbatim from the Python
//! library's `inspect/_page.py`; no build step, no CDN script).
//!
//! Two views over the same `/events` stream: a "Stately" iframe on <https://stately.ai/inspect>
//! driven the way `@statelyai/inspect`'s `BrowserAdapter` drives it (wait for the frame to post
//! `{type: "@statelyai.connected"}`, then `postMessage` every protocol message, deferred ones
//! first), and a minimal local fallback (actor list, current value, event log) that works
//! offline. The script is served from `/app.js` so the CSP can forbid inline JavaScript.

/// The hosted Stately Inspector the page embeds and posts messages to.
pub const STATELY_URL: &str = "https://stately.ai/inspect";

/// The HTML page served at `/` (no inline script: the code lives in [`APP_JS`]).
pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>xsm live inspector</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
 body{font:14px system-ui,sans-serif;margin:0;display:flex;height:100vh}
 #side{width:340px;overflow:auto;border-right:1px solid #ddd;padding:8px}
 #main{flex:1;display:flex;flex-direction:column}
 #stately{flex:1;border:0}
 .ev{font-family:monospace;font-size:12px;padding:1px 0}
 .actor{margin:6px 0;padding:4px;background:#f4f4f8;border-radius:4px}
 .muted{color:#888}
</style></head>
<body>
<div id="side">
 <h3>xsm inspector <span id="status" class="muted">connecting</span></h3>
 <label><input type="checkbox" id="useStately" checked> Stately UI</label>
 <div id="actors"></div><h4>Events</h4><div id="log"></div>
</div>
<div id="main"><iframe id="stately" title="Stately Inspector"
 referrerpolicy="no-referrer"></iframe></div>
<script src="/app.js"></script>
</body></html>
"#;

/// The script served at `/app.js`.
pub const APP_JS: &str = r"'use strict';
(function () {
  var STATELY = 'https://stately.ai/inspect';
  var frame = document.getElementById('stately');
  var connected = false, deferred = [];
  var actors = {};
  function post(msg) {
    if (!document.getElementById('useStately').checked) return;
    if (connected && frame.contentWindow) {
      frame.contentWindow.postMessage(msg, STATELY);
    } else { deferred.push(msg); if (deferred.length > 1000) deferred.shift(); }
  }
  window.addEventListener('message', function (e) {
    if (e.origin !== new URL(STATELY).origin) return;
    if (e.data && e.data.type === '@statelyai.connected') {
      connected = true;
      deferred.splice(0).forEach(post);
    }
  });
  frame.src = STATELY;
  function text(tag, cls, s) {
    var el = document.createElement(tag); el.className = cls;
    el.textContent = s; return el;
  }
  function render() {
    var box = document.getElementById('actors'); box.textContent = '';
    Object.keys(actors).forEach(function (id) {
      var a = actors[id];
      box.appendChild(text('div', 'actor',
        a.name + ' (' + id + ')  ' + JSON.stringify(a.value) +
        '  ' + a.status));
    });
  }
  function local(msg) {
    if (msg.type === '@xstate.actor') {
      actors[msg.sessionId] = {name: msg.name, value: msg.snapshot.value,
                               status: msg.snapshot.status};
    } else if (msg.type === '@xstate.snapshot' && actors[msg.sessionId]) {
      actors[msg.sessionId].value = msg.snapshot.value;
      actors[msg.sessionId].status = msg.snapshot.status;
    } else if (msg.type === '@xstate.event') {
      var who = (msg.sourceId ? msg.sourceId + ' -> ' : '') + msg.sessionId;
      document.getElementById('log').prepend(
        text('div', 'ev', who + '  ' + msg.event.type));
    }
    render();
  }
  var es = new EventSource('/events');
  es.onopen = function () {
    document.getElementById('status').textContent = 'live';
  };
  es.onerror = function () {
    document.getElementById('status').textContent = 'disconnected';
  };
  es.onmessage = function (e) {
    var msg = JSON.parse(e.data); local(msg); post(msg);
  };
})();
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_targets_the_hosted_inspector() {
        assert!(APP_JS.contains(&format!("var STATELY = '{STATELY_URL}';")));
    }

    #[test]
    fn page_has_no_inline_script() {
        assert!(INDEX_HTML.contains(r#"<script src="/app.js"></script>"#));
        assert_eq!(INDEX_HTML.matches("<script").count(), 1);
        assert!(!INDEX_HTML.contains("onclick") && !INDEX_HTML.contains("javascript:"));
    }
}
