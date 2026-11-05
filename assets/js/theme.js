// Theme: runs synchronously in <head> so the stored choice applies before
// first paint. "auto" follows the OS; the server default comes from
// data-default-theme (ui.theme).
(function () {
  "use strict";
  var KEY = "ferrenium-theme";
  var root = document.documentElement;
  var fallback = root.getAttribute("data-default-theme") || "auto";
  var current = fallback;

  try {
    current = localStorage.getItem(KEY) || fallback;
  } catch (e) {}

  function apply(choice) {
    current = choice;
    if (choice === "light" || choice === "dark") {
      root.setAttribute("data-theme", choice);
    } else {
      root.removeAttribute("data-theme");
    }
    var buttons = document.querySelectorAll("[data-theme-choice]");
    for (var i = 0; i < buttons.length; i++) {
      var on = buttons[i].getAttribute("data-theme-choice") === choice;
      buttons[i].setAttribute("aria-pressed", on ? "true" : "false");
    }
  }

  root.classList.add("js");
  apply(current);

  document.addEventListener("DOMContentLoaded", function () {
    apply(current);
    document.addEventListener("click", function (event) {
      var button = event.target.closest && event.target.closest("[data-theme-choice]");
      if (!button) return;
      var choice = button.getAttribute("data-theme-choice");
      try {
        if (choice === fallback) localStorage.removeItem(KEY);
        else localStorage.setItem(KEY, choice);
      } catch (e) {}
      apply(choice);
    });
  });
})();
