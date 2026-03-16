(function () {
  var JWT_SESSION = 'dashboard_jwt';
  var AUTH_URL = 'https://auth-service-rodmen07-v2.fly.dev/dashboard/login';

  var ALL_TABS = [
    { key: 'overview',       label: 'Overview',       href: '/overview',       admin: true  },
    { key: 'gold',           label: 'Gold',           href: '/',               admin: false },
    { key: 'silver',         label: 'Silver',         href: '/silver',         admin: false },
    { key: 'bronze',         label: 'Bronze',         href: '/bronze',         admin: false },
    { key: 'builds',         label: 'Builds',         href: '/builds',         admin: false },
    { key: 'infrastructure', label: 'Infrastructure', href: '/infrastructure', admin: true  },
    { key: 'spend',          label: 'Spend',          href: '/spend',          admin: true  },
    { key: 'messages',       label: 'Messages',       href: '/messages',       admin: true  },
  ];

  function getToken() {
    var hash = window.location.hash;
    if (hash.startsWith('#token=')) {
      var token = hash.slice(7);
      sessionStorage.setItem(JWT_SESSION, token);
      history.replaceState(null, '', window.location.pathname);
      return token;
    }
    return sessionStorage.getItem(JWT_SESSION);
  }

  function isAdmin() {
    var token = getToken();
    if (!token) return false;
    try {
      var parts = token.split('.');
      if (parts.length !== 3) return false;
      var payload = JSON.parse(atob(parts[1].replace(/-/g, '+').replace(/_/g, '/')));
      if (payload.exp && Date.now() / 1000 > payload.exp) return false;
      return Array.isArray(payload.roles) && payload.roles.includes('admin');
    } catch (_) { return false; }
  }

  function renderNav(activeKey) {
    var navEl = document.getElementById('main-nav');
    if (!navEl) return;
    var admin = isAdmin();
    navEl.innerHTML = ALL_TABS
      .filter(function (t) { return admin || !t.admin; })
      .map(function (t) {
        return '<a href="' + t.href + '"' + (t.key === activeKey ? ' class="active"' : '') + '>' + t.label + '</a>';
      })
      .join('');
  }

  function guardAdminPage() {
    if (isAdmin()) return;
    // Hide page body content (keep nav visible)
    Array.from(document.body.children).forEach(function (el) {
      if (el.id !== 'main-nav' && el.tagName.toLowerCase() !== 'nav') {
        el.style.display = 'none';
      }
    });
    // Inject login gate
    var gate = document.createElement('div');
    gate.style.cssText = 'background:#fef3c7;border:1px solid #fbbf24;border-radius:8px;padding:20px 24px;max-width:440px;margin-top:8px';
    gate.innerHTML =
      '<p style="margin:0 0 8px;font-size:.9rem;font-weight:600;color:#78350f">Admin access required</p>' +
      '<p style="margin:0 0 16px;font-size:.85rem;color:#92400e;line-height:1.5">This page is only available to administrators. Sign in with your admin account to continue.</p>' +
      '<a href="' + AUTH_URL + '" style="display:inline-block;padding:7px 18px;background:#1a1a2e;color:#fbbf24;border-radius:6px;font-size:.875rem;text-decoration:none;font-weight:600">Sign in \u2192</a>';
    document.body.appendChild(gate);
  }

  /** fetch() wrapper that injects Authorization: Bearer <token> when a token is present. */
  function authFetch(url, opts) {
    var token = getToken();
    var options = opts || {};
    if (token) {
      options.headers = Object.assign({ 'Authorization': 'Bearer ' + token }, options.headers || {});
    }
    return fetch(url, options);
  }

  window.DashAuth = { getToken: getToken, isAdmin: isAdmin, renderNav: renderNav, guardAdminPage: guardAdminPage, authFetch: authFetch };
}());
