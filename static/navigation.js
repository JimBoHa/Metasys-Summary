"use strict";

(() => {
  let session = null;

  document.addEventListener("DOMContentLoaded", () => {
    document.querySelectorAll("[data-sidebar-toggle]").forEach((button) => button.addEventListener("click", openSidebar));
    document.querySelectorAll("[data-sidebar-close]").forEach((button) => button.addEventListener("click", closeSidebar));
    document.querySelectorAll(".sidebar-group-toggle").forEach((button) => {
      const group = button.closest(".sidebar-group");
      const key = group?.dataset.sidebarGroup;
      if (key && storedGroupState(key) === "collapsed") setGroupCollapsed(group, true);
      button.addEventListener("click", () => {
        const collapsed = !group.classList.contains("collapsed");
        setGroupCollapsed(group, collapsed);
        storeGroupState(key, collapsed ? "collapsed" : "expanded");
      });
    });
    document.querySelectorAll(".sidebar-link").forEach((link) => link.addEventListener("click", closeSidebar));
    document.querySelectorAll(".sidebar-sign-out").forEach((button) => button.addEventListener("click", signOut));
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") closeSidebar();
    });
    setActive(document.body.dataset.navActive || "");
  });

  function configure(nextSession) {
    session = nextSession;
    const role = nextSession?.user?.role || "";
    document.querySelectorAll('[data-sidebar-role="operations"]').forEach((item) => {
      item.hidden = !["admin", "operator"].includes(role);
    });
    document.querySelectorAll('[data-sidebar-role="admin"]').forEach((item) => {
      item.hidden = role !== "admin";
    });
    document.querySelectorAll(".sidebar-user-name").forEach((node) => {
      node.textContent = nextSession?.user?.displayName || "Signed in";
    });
    document.querySelectorAll(".sidebar-user-role").forEach((node) => {
      node.textContent = roleLabel(role);
    });
  }

  function setActive(key) {
    document.querySelectorAll(".sidebar-link").forEach((link) => {
      const active = Boolean(key) && link.dataset.navKey === key;
      link.classList.toggle("active", active);
      if (active) link.setAttribute("aria-current", "page");
      else link.removeAttribute("aria-current");
    });
    const activeLink = document.querySelector(`.sidebar-link[data-nav-key="${cssEscape(key)}"]`);
    const group = activeLink?.closest(".sidebar-group");
    if (group) setGroupCollapsed(group, false);
  }

  function openSidebar() {
    document.body.classList.add("sidebar-open");
    document.querySelector(".primary-sidebar")?.focus();
  }

  function closeSidebar() {
    document.body.classList.remove("sidebar-open");
  }

  function setGroupCollapsed(group, collapsed) {
    if (!group) return;
    group.classList.toggle("collapsed", collapsed);
    group.querySelector(".sidebar-group-toggle")?.setAttribute("aria-expanded", String(!collapsed));
  }

  async function signOut() {
    const button = document.querySelector(".sidebar-sign-out");
    if (button) button.disabled = true;
    try {
      const headers = new Headers();
      if (session?.csrfToken) headers.set("X-CSRF-Token", session.csrfToken);
      await fetch("/api/portal/logout", { method: "POST", headers, credentials: "same-origin" });
    } finally {
      window.location.assign("/");
    }
  }

  function roleLabel(role) {
    if (role === "admin") return "Administrator";
    if (role === "operator") return "Operator";
    if (role === "reportingStaff") return "Reporting staff";
    if (role === "viewOnly") return "View only";
    return role || "Account";
  }

  function storeGroupState(key, value) {
    if (!key) return;
    try {
      window.localStorage.setItem(`metasys.sidebar.${key}`, value);
    } catch (_) {
      // Navigation still works when browser storage is unavailable.
    }
  }

  function storedGroupState(key) {
    try {
      return window.localStorage.getItem(`metasys.sidebar.${key}`);
    } catch (_) {
      return null;
    }
  }

  function cssEscape(value) {
    if (window.CSS?.escape) return window.CSS.escape(value || "");
    return String(value || "").replaceAll('"', '\\"');
  }

  window.MetasysNavigation = { configure, setActive, close: closeSidebar };
})();
