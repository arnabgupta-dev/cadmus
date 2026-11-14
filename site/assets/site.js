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
})();
