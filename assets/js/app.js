// Progressive enhancements. Everything works without this file.
(function () {
  "use strict";

  // Index: instant filter; "/" focuses it, Escape clears it.
  var input = document.querySelector("[data-filter]");
  if (input) {
    var rows = Array.prototype.slice.call(document.querySelectorAll("[data-search]"));
    var sections = Array.prototype.slice.call(document.querySelectorAll("[data-section]"));
    var empty = document.querySelector("[data-filter-empty]");

    var apply = function () {
      var terms = input.value.toLowerCase().split(/\s+/).filter(Boolean);
      var shown = 0;
      rows.forEach(function (row) {
        var text = row.getAttribute("data-search");
        var match = terms.every(function (t) { return text.indexOf(t) !== -1; });
        row.hidden = !match;
        if (match) shown++;
      });
      sections.forEach(function (section) {
        section.hidden = !section.querySelector("[data-search]:not([hidden])");
      });
      if (empty) empty.hidden = shown > 0;
    };

    input.addEventListener("input", apply);
    input.addEventListener("keydown", function (e) {
      if (e.key === "Escape") {
        input.value = "";
        apply();
        input.blur();
      }
    });
    document.addEventListener("keydown", function (e) {
      if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey) return;
      var t = e.target;
      if (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName)) return;
      e.preventDefault();
      input.focus();
      input.select();
    });
  }
  // Blob: line ranges. #L12 works without JS (:target); here shift-click
  // extends to #L12-L20 and the range is highlighted on load.
  var code = document.querySelector("[data-lines]");
  if (code) {
    var anchor = null;
    var select = function (from, to) {
      Array.prototype.forEach.call(code.querySelectorAll("tr.selected"), function (tr) {
        tr.classList.remove("selected");
      });
      if (!from) return;
      if (to < from) { var t = from; from = to; to = t; }
      for (var n = from; n <= to; n++) {
        var row = document.getElementById("L" + n);
        if (row) row.classList.add("selected");
      }
    };
    var fromHash = function () {
      var m = /^#L(\d+)(?:-L(\d+))?$/.exec(location.hash);
      if (!m) return select(0);
      anchor = +m[1];
      select(anchor, +(m[2] || m[1]));
      if (m[2]) {
        var row = document.getElementById("L" + Math.min(+m[1], +m[2]));
        if (row) row.scrollIntoView({ block: "center" });
      }
    };
    code.addEventListener("click", function (e) {
      var a = e.target.closest && e.target.closest(".ln a");
      if (!a) return;
      var n = +a.textContent;
      e.preventDefault();
      var hash = "#L" + n;
      if (e.shiftKey && anchor && anchor !== n) {
        hash = "#L" + Math.min(anchor, n) + "-L" + Math.max(anchor, n);
      } else {
        anchor = n;
      }
      history.replaceState(null, "", hash);
      fromHash();
    });
    window.addEventListener("hashchange", fromHash);
    fromHash();
  }

  // Summary: clone protocol switch and copy button.
  var clone = document.querySelector("[data-clone]");
  if (clone) {
    var KEY = "ferrenium-clone";
    var status = clone.querySelector("[data-copy-status]");
    var choose = function (i) {
      var rows = clone.querySelectorAll("[data-clone-url]");
      if (!rows[i]) i = 0;
      Array.prototype.forEach.call(rows, function (row, j) { row.hidden = j !== i; });
      Array.prototype.forEach.call(clone.querySelectorAll("[data-clone-choice]"), function (b, j) {
        b.setAttribute("aria-pressed", j === i ? "true" : "false");
      });
    };
    var saved = 0;
    try { saved = +(localStorage.getItem(KEY) || 0); } catch (e) {}
    choose(saved);
    clone.addEventListener("click", function (e) {
      var choice = e.target.closest("[data-clone-choice]");
      if (choice) {
        var i = +choice.getAttribute("data-clone-choice");
        choose(i);
        try { localStorage.setItem(KEY, i); } catch (err) {}
        return;
      }
      var copy = e.target.closest("[data-copy]");
      if (!copy) return;
      var input = copy.parentNode.querySelector("input");
      var done = function (ok) {
        status.textContent = ok ? "[Copied]" : "[Error: copy failed]";
        status.classList.toggle("status-error", !ok);
        clearTimeout(done.t);
        done.t = setTimeout(function () { status.textContent = ""; }, 2000);
      };
      if (navigator.clipboard) {
        navigator.clipboard.writeText(input.value).then(function () { done(true); }, function () { done(false); });
      } else {
        input.select();
        done(document.execCommand("copy"));
      }
    });
  }
})();
