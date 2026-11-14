// Cadmus website: theme, menu, copy buttons, code colours, search and the page outline.
(function () {
  var root = document.documentElement;

  // Theme: remembered per browser when the toggle is used, otherwise the device's.
  function isDark() {
    if (root.dataset.theme) return root.dataset.theme === "dark";
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  }
  document.querySelectorAll("[data-theme-toggle]").forEach(function (button) {
    button.addEventListener("click", function () {
      var next = isDark() ? "light" : "dark";
      root.dataset.theme = next;
      try { localStorage.setItem("cadmus-theme", next); } catch (e) {}
    });
  });

  // The page list on phones.
  var menu = document.querySelector("[data-menu]");
  var sidebar = document.getElementById("sidebar");
  if (menu && sidebar) {
    menu.addEventListener("click", function () {
      var open = sidebar.classList.toggle("open");
      menu.setAttribute("aria-expanded", open ? "true" : "false");
    });
    sidebar.addEventListener("click", function (event) {
      if (event.target.closest("a")) sidebar.classList.remove("open");
    });
  }

  function copy(text, button) {
    var done = function () {
      var label = button.textContent;
      button.textContent = "Copied";
      setTimeout(function () { button.textContent = label; }, 1500);
    };
    if (navigator.clipboard) navigator.clipboard.writeText(text).then(done, function () {});
  }
  document.querySelectorAll("[data-copy]").forEach(function (button) {
    button.addEventListener("click", function () { copy(button.dataset.copy, button); });
  });

  // Code blocks: colours, and a copy button on each in the docs.
  document.querySelectorAll("pre code").forEach(function (block) {
    if (window.hljs && /language-(?!text)/.test(block.className)) {
      try { window.hljs.highlightElement(block); } catch (e) {}
    }
    var pre = block.parentElement;
    if (pre.closest(".prose")) {
      var button = document.createElement("button");
      button.type = "button";
      button.className = "copy";
      button.textContent = "Copy";
      button.setAttribute("aria-label", "Copy the code");
      button.addEventListener("click", function () { copy(block.innerText, button); });
      pre.appendChild(button);
    }
  });

  // Search, over docs/search.json, loaded when first needed.
  var input = document.querySelector("[data-search]");
  var results = document.querySelector("[data-results]");
  var index = null;
  var selected = -1;
  function load() {
    if (index) return Promise.resolve(index);
    return fetch("search.json").then(function (r) { return r.json(); }).then(function (data) {
      index = data;
      return index;
    });
  }
  function score(entry, terms) {
    var heading = entry.heading.toLowerCase();
    var page = entry.page.toLowerCase();
    var text = entry.text.toLowerCase();
    var total = 0;
    for (var i = 0; i < terms.length; i++) {
      var t = terms[i];
      var found = false;
      if (heading.indexOf(t) >= 0) { total += 10; found = true; }
      if (page.indexOf(t) >= 0) { total += 4; found = true; }
      var at = text.indexOf(t);
      if (at >= 0) { total += 1 + Math.min(5, text.split(t).length - 1); found = true; }
      if (!found) return 0;
    }
    return total;
  }
  function escapeHtml(s) {
    return s.replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; });
  }
  function render(query) {
    var terms = query.toLowerCase().split(/\s+/).filter(function (t) { return t.length > 1; });
    selected = -1;
    if (!terms.length) { results.hidden = true; results.innerHTML = ""; return; }
    load().then(function (data) {
      var hits = data
        .map(function (entry) { return { entry: entry, score: score(entry, terms) }; })
        .filter(function (hit) { return hit.score > 0; })
        .sort(function (a, b) { return b.score - a.score; })
        .slice(0, 8);
      results.hidden = false;
      if (!hits.length) {
        results.innerHTML = '<li class="empty">Nothing matches. Try other words.</li>';
        return;
      }
      results.innerHTML = hits.map(function (hit) {
        var e = hit.entry;
        var title = e.heading === e.page ? e.page : e.page + " › " + e.heading;
        return '<li><a href="' + e.url + '"><strong>' + escapeHtml(title) + "</strong><span>" + escapeHtml(e.text.slice(0, 120)) + "</span></a></li>";
      }).join("");
    });
  }
  if (input && results) {
    input.addEventListener("input", function () { render(input.value); });
    input.addEventListener("focus", load);
    input.addEventListener("keydown", function (event) {
      var links = results.querySelectorAll("a");
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        if (!links.length) return;
        event.preventDefault();
        selected = (selected + (event.key === "ArrowDown" ? 1 : -1) + links.length) % links.length;
        links.forEach(function (a, i) { a.setAttribute("aria-selected", i === selected ? "true" : "false"); });
      } else if (event.key === "Enter" && links.length) {
        event.preventDefault();
        window.location.href = links[Math.max(selected, 0)].getAttribute("href");
      } else if (event.key === "Escape") {
        input.value = "";
        render("");
        input.blur();
      }
    });
    document.addEventListener("keydown", function (event) {
      var typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);
      if (event.key === "/" && !typing) {
        event.preventDefault();
        if (sidebar && window.matchMedia("(max-width: 900px)").matches) sidebar.classList.add("open");
        input.focus();
      }
    });
  }

  // The outline follows the reading position.
  var outline = document.querySelectorAll(".toc a");
  if (outline.length && "IntersectionObserver" in window) {
    var byId = {};
    outline.forEach(function (a) { byId[a.getAttribute("href").slice(1)] = a; });
    var observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          outline.forEach(function (a) { a.classList.remove("active"); });
          var link = byId[entry.target.id];
          if (link) link.classList.add("active");
        }
      });
    }, { rootMargin: "-80px 0px -70% 0px" });
    document.querySelectorAll(".prose h2[id], .prose h3[id]").forEach(function (h) { observer.observe(h); });
  }

  // The hero demo: types each step's commands, prints their output, then shows the admin
  // they make, and starts over. It waits while paused, scrolled away or in a hidden tab.
  var demo = document.querySelector("[data-demo]");
  if (demo) setupDemo(demo);

  function setupDemo(demo) {
    var screen = demo.querySelector("[data-demo-screen]");
    var buttons = demo.querySelectorAll("[data-step]");
    var pauseButton = demo.querySelector("[data-demo-pause]");
    var steps;
    try { steps = JSON.parse(demo.querySelector("[data-demo-script]").textContent); } catch (e) { return; }
    var still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    var run = 0;
    var paused = false;
    var visible = false;
    var started = false;
    demo.classList.add("live");

    // Resolves after `ms` of running time; rejects once a newer run has started.
    function wait(ms, id) {
      return new Promise(function (resolve, reject) {
        var left = ms;
        (function tick() {
          if (id !== run) return reject(new Error("cancelled"));
          if (paused || !visible || document.hidden) return setTimeout(tick, 200);
          if (left <= 0) return resolve();
          var slice = Math.min(left, 50);
          left -= slice;
          setTimeout(tick, slice);
        })();
      });
    }
    function span(cls, text) {
      var el = document.createElement("span");
      el.className = cls;
      el.textContent = text;
      return el;
    }
    async function print(entry, instant, id) {
      if (screen.childNodes.length) screen.appendChild(document.createTextNode("\n"));
      if (entry[0] === "cmd") {
        screen.appendChild(span("t-p", "$"));
        var typed = document.createTextNode(" ");
        screen.appendChild(typed);
        if (instant) { typed.data += entry[1]; return; }
        var caret = span("demo-caret", "");
        screen.appendChild(caret);
        await wait(400, id);
        for (var i = 0; i < entry[1].length; i++) {
          typed.data += entry[1][i];
          await wait(16 + Math.random() * 38, id);
        }
        await wait(280, id);
        caret.remove();
        return;
      }
      if (!instant) await wait(entry[0] === "ok" ? 160 : 260, id);
      if (entry[0] === "link") {
        screen.appendChild(span("t-o", entry[1]));
        screen.appendChild(document.createTextNode(" "));
        screen.appendChild(span("t-l", entry[2]));
      } else {
        screen.appendChild(span(entry[0] === "ok" ? "t-g" : "t-o", entry[1]));
      }
    }
    function mark(current) {
      buttons.forEach(function (button, i) {
        button.classList.toggle("done", i < current);
        if (i === current) button.setAttribute("aria-current", "step");
        else button.removeAttribute("aria-current");
      });
    }
    async function play(from) {
      var id = ++run;
      screen.textContent = "";
      demo.classList.remove("show-browser");
      try {
        for (var s = 0; s < steps.length; s++) {
          var instant = still || s < from;
          if (s >= from) mark(s);
          for (var j = 0; j < steps[s].lines.length; j++) await print(steps[s].lines[j], instant, id);
          if (instant) continue;
          if (steps[s].browser) {
            await wait(700, id);
            demo.classList.add("show-browser");
          }
          await wait(900, id);
        }
        mark(steps.length);
        if (still) return;
        await wait(6500, id);
        demo.classList.remove("show-browser");
        await wait(900, id);
        play(0);
      } catch (e) {
        // A newer run replaced this one.
      }
    }

    buttons.forEach(function (button) {
      button.addEventListener("click", function () {
        started = true;
        play(Number(button.dataset.step));
      });
    });
    if (pauseButton) {
      pauseButton.addEventListener("click", function () {
        paused = !paused;
        pauseButton.textContent = paused ? "Play" : "Pause";
        pauseButton.setAttribute("aria-label", paused ? "Play the demo" : "Pause the demo");
      });
    }
    function start() {
      if (!started) { started = true; play(0); }
    }
    if ("IntersectionObserver" in window) {
      new IntersectionObserver(function (entries) {
        visible = entries[0].isIntersecting;
        if (visible) start();
      }, { threshold: 0.25 }).observe(demo);
    } else {
      visible = true;
      start();
    }
  }
})();
