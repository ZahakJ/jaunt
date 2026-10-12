// A link back to the jaunt landing page at the left of the menu bar.
(function () {
    var bar = document.querySelector('.left-buttons');
    if (!bar || bar.querySelector('.jaunt-home')) return;
    var a = document.createElement('a');
    a.className = 'jaunt-home';
    a.href = '../';
    a.textContent = 'jaunt';
    a.title = 'jaunt home page';
    bar.appendChild(a);
})();
