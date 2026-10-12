// Fills the download block from the latest GitHub release.
//
// Markup contract (everything degrades to the static links when the API is unreachable):
//   <section id="download" data-release="owner/repo">
//     <a data-release-primary href="https://github.com/owner/repo/releases/latest">Download</a>
//     <span data-release-tag></span> <span data-release-date></span>
//     <ul data-release-assets></ul>
//   </section>
// The page defines window.releaseAssets before this script runs: an ordered list of
//   { test: /regex on the asset name/, os: 'linux'|'mac'|'windows'|'android'|'any', label: 'Linux AppImage' }
// The first entry whose os matches the visitor's platform becomes the primary link; every
// asset that matches an entry is listed under it. Assets matching no entry are not shown.
(function () {
  var root = document.querySelector('[data-release]');
  if (!root) return;
  var repo = root.getAttribute('data-release');
  var table = window.releaseAssets || [];

  function platform() {
    var ua = navigator.userAgent || '';
    var p = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '';
    if (/Android/i.test(ua)) return 'android';
    if (/Win/i.test(p) || /Windows/i.test(ua)) return 'windows';
    if (/Mac/i.test(p) || /Macintosh/i.test(ua)) return 'mac';
    if (/Linux|X11|CrOS/i.test(p) || /Linux/i.test(ua)) return 'linux';
    return 'any';
  }

  function fill(rel) {
    var tag = rel.tag_name || rel.name || '';
    var date = rel.published_at ? new Date(rel.published_at) : null;
    var assets = rel.assets || [];
    var here = platform();

    root.querySelectorAll('[data-release-tag]').forEach(function (el) { el.textContent = tag; });
    if (date) {
      var text = date.toLocaleDateString(undefined, { year: 'numeric', month: 'long', day: 'numeric' });
      root.querySelectorAll('[data-release-date]').forEach(function (el) {
        el.textContent = text;
        if (el.tagName === 'TIME') el.setAttribute('datetime', date.toISOString().slice(0, 10));
      });
    }

    var primary = root.querySelector('[data-release-primary]');
    var chosen = null;
    for (var i = 0; i < table.length && !chosen; i++) {
      var entry = table[i];
      if (entry.os !== here && entry.os !== 'any') continue;
      for (var j = 0; j < assets.length; j++) {
        if (entry.test.test(assets[j].name)) { chosen = { entry: entry, asset: assets[j] }; break; }
      }
    }
    if (primary && chosen) {
      primary.href = chosen.asset.browser_download_url;
      primary.setAttribute('download', '');
      var label = primary.querySelector('[data-release-primary-label]') || primary;
      label.textContent = (primary.getAttribute('data-release-verb') || 'Download') + ' ' + tag + ' · ' + chosen.entry.label;
      var size = root.querySelector('[data-release-primary-size]');
      if (size) size.textContent = (chosen.asset.size / 1048576).toFixed(1) + ' MB';
    }

    var list = root.querySelector('[data-release-assets]');
    if (list) {
      list.textContent = '';
      table.forEach(function (entry) {
        assets.forEach(function (a) {
          if (!entry.test.test(a.name)) return;
          var li = document.createElement('li');
          var link = document.createElement('a');
          link.href = a.browser_download_url;
          link.textContent = entry.label;
          var code = document.createElement('code');
          code.textContent = a.name;
          var sz = document.createElement('span');
          sz.className = 'size';
          sz.textContent = (a.size / 1048576).toFixed(1) + ' MB';
          li.appendChild(link);
          li.appendChild(document.createTextNode(' '));
          li.appendChild(code);
          li.appendChild(document.createTextNode(' '));
          li.appendChild(sz);
          list.appendChild(li);
        });
      });
    }
    root.setAttribute('data-release-state', 'ready');
  }

  fetch('https://api.github.com/repos/' + repo + '/releases/latest', {
    headers: { Accept: 'application/vnd.github+json' }
  })
    .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
    .then(fill)
    .catch(function () { root.setAttribute('data-release-state', 'static'); });
})();
