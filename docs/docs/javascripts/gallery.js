// The plugin gallery: cards from assets/gallery.json, each opening its details and a recording.
(function () {
  var REPO = "https://github.com/leonardcser/smelt/blob/main/";

  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function figure(src) {
    var f = el("figure", "term");
    f.dataset.replay = src;
    f.setAttribute("markdown", "0");
    var pre = el("pre", "term__screen");
    pre.setAttribute("aria-hidden", "true");
    f.appendChild(pre);
    return f;
  }

  // Where there is a pointer, a card plays while hovered or focused; without one, while on screen.
  var hovers = window.matchMedia("(hover: hover)").matches;

  function init() {
    var root = document.getElementById("gallery");
    if (!root || !window.smeltReplay) return;
    var base = root.dataset.base;
    // The posters are one file, and a card draws its own only while it is near the screen.
    var fetched = [fetch(root.dataset.items), hovers ? fetch(base + "stills.json") : Promise.resolve(null)];
    Promise.all(fetched.map(function (p) { return p.then(function (r) { return r && r.json(); }); })).then(function (loaded) {
      build(root, base, loaded[0], loaded[1]);
    });
  }

  function build(root, base, data, stills) {
    var items = data.items;
    var bySlug = {};
    items.forEach(function (i) { bySlug[i.slug] = i; });
    var active = "All";
    var query = "";

    var bar = el("div", "gallery__bar");
    var search = el("input", "gallery__search");
    search.type = "search";
    search.placeholder = "Search " + items.length + " plugins";
    search.setAttribute("aria-label", "Search plugins");
    var chips = el("div", "gallery__chips");
    bar.append(search, chips);

    var empty = el("p", "gallery__empty", "No plugin matches.");
    empty.hidden = true;
    var dialog = el("dialog", "gallery__dialog");

    function chip(name) {
      var n = name === "All" ? items.length : items.filter(function (i) { return i.category === name; }).length;
      var b = el("button", "gallery__chip", name + " ");
      b.type = "button";
      b.appendChild(el("span", "", String(n)));
      b.setAttribute("aria-pressed", name === active ? "true" : "false");
      b.onclick = function () { choose(name); };
      return b;
    }

    function choose(name) {
      active = name;
      query = search.value = "";
      refresh();
      root.scrollIntoView({ block: "start" });
    }

    function card(item) {
      var a = el("a", "gallery__card");
      a.href = "#" + item.slug;
      a.dataset.slug = item.slug;
      var thumb = el("div", "gallery__thumb");
      var fig = figure(base + item.slug + ".json");
      if (hovers) {
        fig.setAttribute("data-hover", "");
        fig.setAttribute("data-lazy", "");
        fig.__data = stills[item.slug];
      }
      thumb.appendChild(fig);
      // The caption is the name and the command; the summary is in the tooltip and the dialog.
      a.title = item.summary;
      var body = el("div", "gallery__body");
      body.appendChild(el("strong", "", item.title));
      if (item.command) body.appendChild(el("code", "gallery__command", item.command));
      a.append(thumb, body);
      if (hovers) {
        var play = function () {
          var r = fig.__replay;
          if (!r || r.hovered) return;
          r.hovered = true;
          if (!r.full) {
            r.full = true;
            fig.__data = null;
            r.loading = null;
            r.frames = null;
          }
          r.visible = true;
          r.play();
        };
        var stop = function () {
          var r = fig.__replay;
          if (!r || !r.hovered) return;
          r.hovered = false;
          r.pause();
          // Back to the poster, which is the still the card started with.
          if (r.full) {
            r.full = false;
            fig.__data = stills[item.slug];
            r.loading = null;
            r.frames = null;
            r.load();
          }
        };
        a.addEventListener("mouseenter", play);
        a.addEventListener("focus", play);
        a.addEventListener("mouseleave", stop);
        a.addEventListener("blur", stop);
      }
      a.addEventListener("click", function (e) {
        e.preventDefault();
        open(item);
      });
      return a;
    }

    // Every section is a heading, a line on what it holds and a connected grid of cards. The first,
    // Featured, repeats cards from the others.
    var sections = [{ name: "Featured", blurb: "A few to try first.", featured: true, items: data.featured.map(function (s) { return bySlug[s]; }) }].concat(
      data.sections.map(function (s) {
        return { name: s.name, blurb: s.blurb, items: items.filter(function (i) { return i.category === s.name; }) };
      }),
    );
    sections.forEach(function (sec) {
      sec.el = el("section", "gallery__section");
      var head = el("header", "gallery__title");
      head.append(el("h2", "", sec.name), el("p", "", sec.blurb));
      var grid = el("div", "gallery__grid");
      sec.cards = sec.items.map(function (item) {
        var c = card(item);
        grid.appendChild(c);
        return { item: item, el: c };
      });
      sec.el.append(head, grid);
    });

    root.replaceChildren.apply(root, [bar].concat(sections.map(function (s) { return s.el; }), [empty, dialog]));
    // Each card's recording loads and plays only while the card is on screen or hovered.
    root.querySelectorAll(".gallery__card figure").forEach(function (f) { window.smeltReplay.mount(f); });

    function refresh() {
      chips.replaceChildren.apply(chips, ["All"].concat(data.sections.map(function (s) { return s.name; })).map(chip));
      var q = query.trim().toLowerCase();
      var shown = 0;
      sections.forEach(function (sec) {
        var any = 0;
        sec.cards.forEach(function (c) {
          var hit = q
            ? !sec.featured && (c.item.title + " " + c.item.summary + " " + (c.item.command || "") + " " + c.item.category).toLowerCase().indexOf(q) >= 0
            : sec.featured ? active === "All" : active === "All" || c.item.category === active;
          c.el.hidden = !hit;
          if (hit) any++;
        });
        sec.el.hidden = any === 0;
        if (!sec.featured) shown += any;
      });
      // A search lists its matches without the section headings.
      root.classList.toggle("is-searching", q !== "");
      empty.hidden = shown > 0;
    }
    search.oninput = function () { query = search.value; refresh(); };
    refresh();

    function close() {
      if (dialog.open) dialog.close();
    }
    dialog.addEventListener("close", function () {
      var live = dialog.querySelector("figure");
      if (live && window.smeltReplay) window.smeltReplay.unmount(live);
      dialog.replaceChildren();
      if (location.hash) history.replaceState(null, "", location.pathname + location.search);
    });
    dialog.addEventListener("click", function (e) {
      if (e.target === dialog) close();
    });

    function open(item) {
      var head = el("header", "gallery__head");
      var close_ = el("button", "gallery__close", "×");
      close_.type = "button";
      close_.setAttribute("aria-label", "Close");
      close_.onclick = close;
      head.append(el("h2", "", item.title), close_);

      var facts = [item.category];
      if (item.command) facts.push(item.command);
      if (item.bundled) facts.push("bundled");
      var tags = el("p", "gallery__tags", facts.join(" \u00b7 "));

      var body = el("div", "gallery__details");
      body.append(head, tags, el("p", "", item.summary));
      if (item.detail) body.appendChild(el("p", "", item.detail));
      if (item.source) {
        var link = el("a", "", "View source");
        link.href = REPO + item.source;
        body.appendChild(el("p", "", "")).appendChild(link);
      }
      var live = figure(base + item.slug + ".json");
      dialog.replaceChildren(live, body);
      window.smeltReplay.mount(live);
      if (!dialog.open) dialog.showModal();
      history.replaceState(null, "", "#" + item.slug);
    }

    var wanted = items.filter(function (i) { return "#" + i.slug === location.hash; })[0];
    if (wanted) open(wanted);
  }

  if (window.document$) window.document$.subscribe(init);
  else init();
})();
