// Project page behaviour, shared by rulogman, rudbman and rudbgen: keep the
// three copies identical. Everything here is an enhancement; the page reads
// fine without it.
(function () {
  "use strict";

  // The latest release tag on the download button.
  var repo = document.documentElement.getAttribute("data-repo");
  if (repo && window.fetch) {
    fetch("https://api.github.com/repos/" + repo + "/releases/latest")
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (release) {
        if (!release || !release.tag_name) return;
        document.querySelectorAll("[data-version]").forEach(function (el) {
          el.textContent = release.tag_name;
          el.hidden = false;
        });
      })
      .catch(function () {});
  }

  // Install tabs, opened on the visitor's own platform.
  var tabs = Array.prototype.slice.call(document.querySelectorAll('.tabs [role="tab"]'));
  function select(tab) {
    tabs.forEach(function (t) {
      var on = t === tab;
      t.setAttribute("aria-selected", on ? "true" : "false");
      t.tabIndex = on ? 0 : -1;
      document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
    });
  }
  tabs.forEach(function (tab, i) {
    tab.addEventListener("click", function () { select(tab); });
    tab.addEventListener("keydown", function (e) {
      var step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (!step) return;
      var next = tabs[(i + step + tabs.length) % tabs.length];
      select(next);
      next.focus();
    });
  });
  var platform = ((navigator.userAgentData && navigator.userAgentData.platform) ||
    navigator.platform || navigator.userAgent || "").toLowerCase();
  var os = /mac|iphone|ipad/.test(platform) ? "macos"
    : /win/.test(platform) ? "windows"
    : /linux|x11/.test(platform) ? "linux" : null;
  var mine = os && document.getElementById("tab-" + os);
  if (mine) select(mine);

  // Copy the commands of a terminal block, without prompts or comments.
  document.querySelectorAll(".copy").forEach(function (button) {
    button.addEventListener("click", function () {
      var lines = button.parentNode.querySelectorAll(".c");
      var text = Array.prototype.map.call(lines, function (l) { return l.textContent; }).join("\n");
      if (!navigator.clipboard) return;
      navigator.clipboard.writeText(text).then(function () {
        button.textContent = "Copied";
        button.classList.add("done");
        setTimeout(function () {
          button.textContent = "Copy";
          button.classList.remove("done");
        }, 1600);
      });
    });
  });
})();
