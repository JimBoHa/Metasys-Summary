"use strict";

const SVG_NS = "http://www.w3.org/2000/svg";
const app = {
  session: null,
  map: null,
  metasysSettings: null,
  localConfigurationAllowed: false,
  users: [],
  requests: [],
  activeView: "home",
  activeBuildingId: null,
  activeFloorId: null,
  selectedRequestId: null,
  editor: {
    key: null,
    plan: null,
    trace: [],
    regions: [],
    tool: "select",
    selection: null,
    pendingPoints: [],
    draftRegion: null,
    drag: null
  }
};

const $ = (id) => document.getElementById(id);

document.addEventListener("DOMContentLoaded", async () => {
  bindEvents();
  await initializePortal();
});

function bindEvents() {
  $("setup-metasys-form").addEventListener("submit", (event) => saveMetasysConnection(event, "setup"));
  $("setup-form").addEventListener("submit", bootstrapAdministrator);
  $("login-form").addEventListener("submit", signIn);
  $("logout-button").addEventListener("click", signOut);
  document.querySelectorAll(".nav-button").forEach((button) => {
    button.addEventListener("click", () => showView(button.dataset.view));
  });
  $("refresh-map-button").addEventListener("click", () => loadMap(true));
  $("refresh-requests-button").addEventListener("click", loadRequests);
  document.querySelectorAll(".admin-tab").forEach((button) => {
    button.addEventListener("click", () => showAdminPane(button.dataset.adminView));
  });

  $("building-form").addEventListener("submit", saveBuilding);
  $("building-clear").addEventListener("click", clearBuildingForm);
  $("floor-form").addEventListener("submit", saveFloor);
  $("floor-clear").addEventListener("click", clearFloorForm);

  $("plan-scope-type").addEventListener("change", updatePlanScopeOptions);
  $("plan-pdf").addEventListener("change", updateFileLabel);
  $("floorplan-upload-form").addEventListener("submit", uploadFloorPlan);
  $("editor-plan-select").addEventListener("change", (event) => selectEditorPlan(event.target.value));
  $("show-original-plan").addEventListener("change", renderEditor);
  document.querySelectorAll(".tool-button").forEach((button) => {
    button.addEventListener("click", () => setEditorTool(button.dataset.tool));
  });
  $("finish-region-button").addEventListener("click", finishRegionBoundary);
  $("delete-selection-button").addEventListener("click", deleteEditorSelection);
  $("save-trace-button").addEventListener("click", saveTrace);
  $("region-form").addEventListener("submit", saveRegion);
  $("region-cancel").addEventListener("click", cancelRegionForm);

  $("user-form").addEventListener("submit", saveUser);
  $("user-clear").addEventListener("click", clearUserForm);
  $("user-role").addEventListener("change", updateUserScopeVisibility);
  $("admin-metasys-form").addEventListener("submit", (event) => saveMetasysConnection(event, "admin"));

  $("report-form").addEventListener("submit", submitServiceRequest);
  $("report-close").addEventListener("click", closeReportDialog);
  $("report-cancel").addEventListener("click", closeReportDialog);
}

async function initializePortal() {
  try {
    const status = await request("/api/portal/status", { allowUnauthorized: true });
    app.localConfigurationAllowed = Boolean(status.localConfigurationAllowed);
    if (!status.initialized) {
      $("setup-notice").hidden = false;
      $("setup-metasys-form").hidden = !status.bootstrapAllowed;
      $("setup-form").hidden = !status.bootstrapAllowed;
      $("setup-local-only").hidden = status.bootstrapAllowed;
      $("login-form").hidden = true;
      if (status.bootstrapAllowed) await loadMetasysSettings("setup", true);
      return;
    }
    try {
      const session = await request("/api/portal/me", { allowUnauthorized: true });
      await enterPortal(session);
    } catch (error) {
      if (error.status !== 401) showLoginMessage(error.message, true);
    }
  } catch (error) {
    showLoginMessage(error.message || "Portal service unavailable", true);
  }
}

async function loadMetasysSettings(prefix, allowUnauthorized = false) {
  try {
    const settings = await request("/api/portal/metasys-settings", { allowUnauthorized });
    app.metasysSettings = settings;
    populateMetasysForm(prefix, settings);
    showFormMessage(`${prefix}-metasys-message`, "");
    return settings;
  } catch (error) {
    showFormMessage(`${prefix}-metasys-message`, error.message || "Unable to load Metasys settings", true);
    return null;
  }
}

function populateMetasysForm(prefix, settings) {
  $(`${prefix}-metasys-server-url`).value = settings.serverUrl || "";
  $(`${prefix}-metasys-username`).value = settings.username || "";
  $(`${prefix}-metasys-domain`).value = settings.domain || "Metasys Local";
  $(`${prefix}-metasys-connector`).value = settings.connector || "auto";
  $(`${prefix}-metasys-api-version`).value = settings.apiVersion || "auto";
  $(`${prefix}-metasys-invalid-certificates`).checked = Boolean(settings.acceptInvalidCertificates);
  setText(
    `${prefix}-metasys-password-state`,
    settings.passwordConfigured ? "Password saved in macOS Keychain." : "No password saved for this connection."
  );
}

async function saveMetasysConnection(event, prefix) {
  event.preventDefault();
  const form = event.currentTarget;
  if (!form.reportValidity()) return;
  const password = $(`${prefix}-metasys-password`).value;
  const passwordConfirmation = $(`${prefix}-metasys-password-confirmation`).value;
  if (password !== passwordConfirmation) {
    showFormMessage(`${prefix}-metasys-message`, "Password confirmation does not match.", true);
    return;
  }
  const button = $(`${prefix}-metasys-button`);
  button.disabled = true;
  showFormMessage(`${prefix}-metasys-message`, "Testing the live Metasys connection… Approve macOS Keychain access if prompted.");
  try {
    const result = await request("/api/portal/metasys-settings", {
      method: "PUT",
      body: {
        serverUrl: $(`${prefix}-metasys-server-url`).value,
        username: $(`${prefix}-metasys-username`).value,
        password,
        passwordConfirmation,
        domain: $(`${prefix}-metasys-domain`).value,
        connector: $(`${prefix}-metasys-connector`).value,
        apiVersion: $(`${prefix}-metasys-api-version`).value,
        acceptInvalidCertificates: $(`${prefix}-metasys-invalid-certificates`).checked
      },
      allowUnauthorized: !app.session
    });
    app.metasysSettings = result.settings;
    populateMetasysForm(prefix, result.settings);
    $(`${prefix}-metasys-password`).value = "";
    $(`${prefix}-metasys-password-confirmation`).value = "";
    const version = result.serverVersion ? ` · server ${result.serverVersion}` : "";
    showFormMessage(
      `${prefix}-metasys-message`,
      `Connected through ${result.connector}${version}. Loaded ${result.alarmRecords} real alarm records and ${result.overrides} overrides.`
    );
    if (app.session) {
      await loadMap();
      showGlobalMessage("Metasys connection tested, saved, and activated.");
    }
  } catch (error) {
    showFormMessage(`${prefix}-metasys-message`, error.message || "Unable to configure Metasys", true);
  } finally {
    button.disabled = false;
  }
}

async function bootstrapAdministrator(event) {
  event.preventDefault();
  const button = $("setup-button");
  const password = $("setup-password").value;
  const passwordConfirmation = $("setup-password-confirmation").value;
  if (password !== passwordConfirmation) {
    showFormMessage("setup-message", "Password confirmation does not match.", true);
    return;
  }
  button.disabled = true;
  showFormMessage("setup-message", "Creating administrator…");
  try {
    const session = await request("/api/portal/bootstrap", {
      method: "POST",
      body: {
        displayName: $("setup-display-name").value,
        email: $("setup-email").value,
        password,
        passwordConfirmation
      },
      allowUnauthorized: true
    });
    $("setup-password").value = "";
    $("setup-password-confirmation").value = "";
    await enterPortal(session);
  } catch (error) {
    showFormMessage("setup-message", error.message || "Unable to create administrator", true);
  } finally {
    button.disabled = false;
  }
}

async function signIn(event) {
  event.preventDefault();
  const button = $("login-button");
  button.disabled = true;
  showLoginMessage("Signing in…");
  try {
    const session = await request("/api/portal/login", {
      method: "POST",
      body: {
        email: $("login-email").value,
        password: $("login-password").value
      },
      allowUnauthorized: true
    });
    $("login-password").value = "";
    await enterPortal(session);
  } catch (error) {
    showLoginMessage(error.message || "Unable to sign in", true);
  } finally {
    button.disabled = false;
  }
}

async function enterPortal(session) {
  app.session = session;
  $("login-view").hidden = true;
  $("app-shell").hidden = false;
  setText("current-user-name", session.user.displayName);
  setText("current-user-role", roleLabel(session.user.role));
  const isAdmin = session.user.role === "admin";
  const isOperator = session.user.role === "operator";
  $("admin-nav").hidden = !isAdmin;
  $("operations-link").hidden = !(isAdmin || isOperator);
  $("admin-metasys-tab").hidden = !isAdmin || !app.localConfigurationAllowed;
  await Promise.all([
    loadMap(),
    loadRequests(),
    isAdmin ? loadUsers() : Promise.resolve(),
    isAdmin && app.localConfigurationAllowed ? loadMetasysSettings("admin") : Promise.resolve()
  ]);
  showView("home");
}

async function signOut() {
  try {
    await request("/api/portal/logout", { method: "POST" });
  } catch (_) {
    // The local UI still clears itself if the session already expired.
  }
  window.location.assign("/");
}

function showView(name) {
  if (name === "admin" && app.session?.user.role !== "admin") return;
  app.activeView = name;
  $("home-section").hidden = name !== "home";
  $("requests-section").hidden = name !== "requests";
  $("admin-section").hidden = name !== "admin";
  document.querySelectorAll(".nav-button").forEach((button) => {
    button.classList.toggle("active", button.dataset.view === name);
  });
  if (name === "requests") renderRequests();
  if (name === "admin") renderAdmin();
  window.location.hash = name;
}

function showAdminPane(name) {
  if (name === "metasys" && !app.localConfigurationAllowed) return;
  $("admin-structure").hidden = name !== "structure";
  $("admin-maps").hidden = name !== "maps";
  $("admin-users").hidden = name !== "users";
  $("admin-metasys").hidden = name !== "metasys";
  document.querySelectorAll(".admin-tab").forEach((button) => {
    button.classList.toggle("active", button.dataset.adminView === name);
  });
  if (name === "maps") renderMapAdministration();
  if (name === "users") renderUsers();
  if (name === "metasys") loadMetasysSettings("admin");
}

async function loadMap(showSuccess = false) {
  const button = $("refresh-map-button");
  button.disabled = true;
  try {
    app.map = await request("/api/portal/map");
    normalizeActiveMapSelection();
    renderHome();
    if (app.session?.user.role === "admin") renderAdmin();
    if (showSuccess) showGlobalMessage("Live space values refreshed.");
  } catch (error) {
    showGlobalMessage(error.message || "Unable to load building spaces", true);
  } finally {
    button.disabled = false;
  }
}

function normalizeActiveMapSelection() {
  const buildings = app.map?.buildings || [];
  if (!buildings.some((building) => building.id === app.activeBuildingId)) {
    app.activeBuildingId = buildings[0]?.id || null;
  }
  const building = activeBuilding();
  if (!building?.floors.some((floor) => floor.id === app.activeFloorId)) {
    app.activeFloorId = building?.floors[0]?.id || null;
  }
}

function activeBuilding() {
  return app.map?.buildings.find((building) => building.id === app.activeBuildingId) || null;
}

function activeFloor() {
  return activeBuilding()?.floors.find((floor) => floor.id === app.activeFloorId) || null;
}

function renderHome() {
  const buildings = app.map?.buildings || [];
  const tabs = $("building-tabs");
  tabs.replaceChildren();
  buildings.forEach((building) => {
    const button = makeButton(building.name, building.id === app.activeBuildingId);
    button.addEventListener("click", () => {
      app.activeBuildingId = building.id;
      app.activeFloorId = building.floors[0]?.id || null;
      renderHome();
    });
    tabs.append(button);
  });
  const building = activeBuilding();
  if (!building) {
    setText("home-title", "No assigned spaces");
    setText("overview-title", "No building configured");
    setText("overview-floor-count", "0 floors");
    renderMapEmpty($("building-overview-map"), "An administrator has not assigned a building or region to this account.");
    $("floor-cards").replaceChildren();
    $("floor-section").hidden = true;
    return;
  }
  setText("home-title", building.name);
  setText("overview-title", building.name);
  setText("overview-floor-count", `${building.floors.length} floor${building.floors.length === 1 ? "" : "s"}`);
  renderPlan($("building-overview-map"), building.overviewPlan, []);
  const cards = $("floor-cards");
  cards.replaceChildren();
  building.floors.forEach((floor) => {
    const button = element("button", "floor-card");
    button.type = "button";
    button.append(element("strong", "", floor.name));
    button.append(element("span", "", `${floor.regions.length} assigned area${floor.regions.length === 1 ? "" : "s"}`));
    button.addEventListener("click", () => {
      app.activeFloorId = floor.id;
      renderFloor();
      $("floor-section").scrollIntoView({ behavior: "smooth", block: "start" });
    });
    cards.append(button);
  });
  renderFloor();
}

function renderFloor() {
  const building = activeBuilding();
  const floor = activeFloor();
  $("floor-section").hidden = !floor;
  if (!floor) return;
  setText("floor-title", `${building.name} · ${floor.name}`);
  const tabs = $("floor-tabs");
  tabs.replaceChildren();
  building.floors.forEach((item) => {
    const button = makeButton(item.name, item.id === floor.id);
    button.addEventListener("click", () => {
      app.activeFloorId = item.id;
      renderFloor();
    });
    tabs.append(button);
  });
  renderPlan($("floor-map"), floor.floorPlan, floor.regions, {
    onRegion: showRegionDetails
  });
  const panel = $("region-panel");
  panel.replaceChildren(
    element("p", "eyebrow", "SELECT AN AREA"),
    element("h3", "", "Live room conditions"),
    element("p", "", "Click a highlighted region on the map to inspect its mapped FAV temperature.")
  );
}

function showRegionDetails(region) {
  const panel = $("region-panel");
  panel.replaceChildren();
  panel.append(element("p", "eyebrow", "LIVE AREA"));
  panel.append(element("h3", "", region.name));
  const temperature = element("div", "temperature-value");
  const reading = region.temperature;
  const value = reading?.available
    ? `${reading.displayValue}${reading.unit ? ` ${reading.unit}` : ""}`
    : "Unavailable";
  temperature.append(element("strong", "", value));
  temperature.append(element("span", "", reading?.available ? `${reading.status} · updated ${relativeTime(reading.observedAt)}` : (reading?.error || "No Metasys point mapped")));
  panel.append(temperature);
  const meta = element("div", "region-meta");
  meta.append(element("span", "", "FAV BOX"));
  meta.append(element("strong", "", region.favBox || "Not assigned"));
  panel.append(meta);
  if (app.map?.canReport) {
    const button = element("button", "primary-button", "Report an issue");
    button.type = "button";
    button.addEventListener("click", () => openReportDialog(region));
    panel.append(button);
  } else {
    panel.append(element("p", "help-text", "This account can view conditions but cannot create service requests."));
  }
}

function renderPlan(container, plan, regions, options = {}) {
  container.replaceChildren();
  if (!plan) {
    renderMapEmpty(container, "No drawing has been uploaded for this space.");
    return null;
  }
  const svg = svgElement("svg");
  svg.setAttribute("viewBox", `0 0 ${plan.width} ${plan.height}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", plan.name);
  const image = svgElement("image", "plan-image");
  image.classList.add("visible-original");
  image.setAttribute("href", plan.imageUrl);
  image.setAttribute("width", String(plan.width));
  image.setAttribute("height", String(plan.height));
  image.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.append(image);
  (plan.trace || []).forEach((feature) => svg.append(traceNode(feature, plan)));
  (regions || []).forEach((region) => {
    const polygon = regionNode(region, plan);
    if (options.onRegion) {
      polygon.setAttribute("tabindex", "0");
      polygon.addEventListener("click", (event) => {
        event.stopPropagation();
        svg.querySelectorAll(".region-shape").forEach((item) => item.classList.remove("selected"));
        polygon.classList.add("selected");
        options.onRegion(region);
      });
      polygon.addEventListener("keydown", (event) => {
        if (event.key === "Enter" || event.key === " ") polygon.dispatchEvent(new MouseEvent("click"));
      });
    }
    svg.append(polygon);
    if (region.temperature) svg.append(temperatureOverlay(region, plan));
  });
  container.append(svg);
  return svg;
}

function temperatureOverlay(region, plan) {
  const center = region.polygon.reduce(
    (total, point) => ({ x: total.x + point.x, y: total.y + point.y }),
    { x: 0, y: 0 }
  );
  const count = Math.max(1, region.polygon.length);
  const reading = region.temperature;
  const label = svgElement("text", `temperature-overlay${reading.available ? "" : " unavailable"}`);
  label.setAttribute("x", String((center.x / count) * plan.width));
  label.setAttribute("y", String((center.y / count) * plan.height));
  label.setAttribute("text-anchor", "middle");
  label.setAttribute("dominant-baseline", "central");
  label.setAttribute("font-size", String(clamp(plan.width / 55, 12, 24)));
  label.setAttribute("aria-hidden", "true");
  label.textContent = reading.available
    ? `${reading.displayValue}${reading.unit ? ` ${reading.unit}` : ""}`
    : "NO DATA";
  return label;
}

function traceNode(feature, plan) {
  const polyline = svgElement("polyline", `trace-feature ${feature.kind}`);
  polyline.setAttribute("points", pointsAttribute(feature.points, plan));
  polyline.setAttribute("stroke-width", String(feature.thickness || 1));
  polyline.dataset.id = feature.id;
  return polyline;
}

function regionNode(region, plan) {
  const polygon = svgElement("polygon", "region-shape");
  polygon.setAttribute("points", pointsAttribute(region.polygon, plan));
  polygon.setAttribute("fill", region.color);
  polygon.setAttribute("fill-opacity", ".2");
  polygon.setAttribute("stroke", region.color);
  polygon.dataset.id = region.id;
  return polygon;
}

function pointsAttribute(points, plan) {
  return (points || []).map((point) => `${point.x * plan.width},${point.y * plan.height}`).join(" ");
}

function renderMapEmpty(container, message) {
  container.replaceChildren(element("p", "map-empty", message));
}

async function loadRequests() {
  try {
    app.requests = await request("/api/portal/requests");
    if (!app.requests.some((item) => item.id === app.selectedRequestId)) {
      app.selectedRequestId = app.requests[0]?.id || null;
    }
    renderRequests();
  } catch (error) {
    showGlobalMessage(error.message || "Unable to load service requests", true);
  }
}

function renderRequests() {
  const list = $("request-list");
  list.replaceChildren();
  if (!app.requests.length) {
    list.append(element("p", "empty-state", "No service requests in the spaces assigned to you."));
    renderRequestDetail();
    return;
  }
  app.requests.forEach((requestItem) => {
    const button = element("button", `request-card${requestItem.id === app.selectedRequestId ? " active" : ""}`);
    button.type = "button";
    const top = element("span");
    top.append(element("strong", "", issueLabel(requestItem.issueType)));
    top.append(element("span", `status-chip ${requestItem.status}`, statusLabel(requestItem.status)));
    button.append(top, element("p", "", requestItem.regionName));
    button.append(element("small", "", `${requestItem.buildingName} · ${requestItem.floorName} · ${relativeTime(requestItem.createdAt)}`));
    button.addEventListener("click", () => {
      app.selectedRequestId = requestItem.id;
      renderRequests();
    });
    list.append(button);
  });
  renderRequestDetail();
}

function renderRequestDetail() {
  const container = $("request-detail");
  container.replaceChildren();
  const item = app.requests.find((requestItem) => requestItem.id === app.selectedRequestId);
  if (!item) {
    container.append(element("p", "empty-state", "Select a request to view details."));
    return;
  }
  container.append(element("p", "eyebrow", "SERVICE REQUEST"));
  container.append(element("h2", "", issueLabel(item.issueType)));
  container.append(element("span", `status-chip ${item.status}`, statusLabel(item.status)));
  const grid = element("div", "detail-grid");
  grid.append(detailItem("Area", item.regionName));
  grid.append(detailItem("Location", `${item.buildingName} · ${item.floorName}`));
  grid.append(detailItem("Contact", item.contactEmail));
  grid.append(detailItem("Reported by", item.createdByName));
  container.append(grid);
  container.append(element("p", "request-description", item.details || "No additional details were provided."));
  container.append(element("h3", "", "Operator notes"));
  const notes = element("div", "notes-list");
  if (!item.notes.length) notes.append(element("p", "help-text", "No operator notes yet."));
  item.notes.forEach((note) => {
    const card = element("div", "note-card");
    card.append(element("strong", "", note.authorName));
    card.append(element("p", "", note.note));
    card.append(element("small", "", formatTime(note.createdAt)));
    notes.append(card);
  });
  container.append(notes);
  if (app.map?.canNote) container.append(operatorRequestControls(item));
}

function operatorRequestControls(item) {
  const wrapper = element("div", "note-form");
  const statusLabelElement = element("label", "", "Request status");
  const select = element("select");
  ["open", "inProgress", "resolved", "closed"].forEach((status) => {
    const option = element("option", "", statusLabel(status));
    option.value = status;
    option.selected = status === item.status;
    select.append(option);
  });
  statusLabelElement.append(select);
  const statusButton = element("button", "secondary-button", "Update status");
  statusButton.type = "button";
  statusButton.addEventListener("click", async () => {
    await runButton(statusButton, async () => {
      const updated = await request(`/api/portal/requests/${encodeURIComponent(item.id)}/status`, {
        method: "PUT",
        body: { status: select.value }
      });
      replaceRequest(updated);
      renderRequests();
      showGlobalMessage("Request status updated.");
    });
  });
  const noteLabel = element("label", "", "Add operator note");
  const textarea = element("textarea");
  textarea.rows = 4;
  textarea.maxLength = 4000;
  noteLabel.append(textarea);
  const noteButton = element("button", "primary-button", "Add note");
  noteButton.type = "button";
  noteButton.addEventListener("click", async () => {
    if (!textarea.value.trim()) return;
    await runButton(noteButton, async () => {
      const updated = await request(`/api/portal/requests/${encodeURIComponent(item.id)}/notes`, {
        method: "POST",
        body: { note: textarea.value }
      });
      replaceRequest(updated);
      renderRequests();
      showGlobalMessage("Operator note added.");
    });
  });
  wrapper.append(statusLabelElement, statusButton, noteLabel, noteButton);
  return wrapper;
}

function replaceRequest(updated) {
  const index = app.requests.findIndex((item) => item.id === updated.id);
  if (index >= 0) app.requests[index] = updated;
}

function openReportDialog(region) {
  $("report-region-id").value = region.id;
  setText("report-region-name", `Report an issue · ${region.name}`);
  $("report-contact-email").value = app.session.user.email;
  $("report-issue-type").value = "too_hot";
  $("report-details").value = "";
  showFormMessage("report-message", "");
  $("report-dialog").showModal();
}

function closeReportDialog() {
  $("report-dialog").close();
}

async function submitServiceRequest(event) {
  event.preventDefault();
  const form = $("report-form");
  if (!form.reportValidity()) return;
  const submit = form.querySelector("button[type=submit]");
  await runButton(submit, async () => {
    try {
      const created = await request("/api/portal/requests", {
        method: "POST",
        body: {
          regionId: $("report-region-id").value,
          contactEmail: $("report-contact-email").value,
          issueType: $("report-issue-type").value,
          details: $("report-details").value
        }
      });
      app.requests.unshift(created);
      app.selectedRequestId = created.id;
      closeReportDialog();
      showGlobalMessage("Service request submitted.");
    } catch (error) {
      showFormMessage("report-message", error.message, true);
    }
  });
}

function renderAdmin() {
  if (app.session?.user.role !== "admin" || !app.map) return;
  renderStructureAdministration();
  renderMapAdministration();
  renderUsers();
}

function renderStructureAdministration() {
  fillBuildingSelect($("floor-building"));
  const container = $("structure-list");
  container.replaceChildren();
  app.map.buildings.forEach((building) => {
    const group = element("section", "structure-building");
    const header = element("header");
    header.append(element("strong", "", building.name));
    const edit = element("button", "mini-button", "Edit building");
    edit.type = "button";
    edit.addEventListener("click", () => editBuilding(building));
    header.append(edit);
    const floors = element("div", "structure-floors");
    building.floors.forEach((floor) => {
      const row = element("div", "structure-floor");
      row.append(element("span", "", floor.name));
      const floorEdit = element("button", "mini-button", "Edit");
      floorEdit.type = "button";
      floorEdit.addEventListener("click", () => editFloor(floor));
      row.append(floorEdit);
      floors.append(row);
    });
    if (!building.floors.length) floors.append(element("p", "help-text", "No floors yet."));
    group.append(header, floors);
    container.append(group);
  });
  if (!app.map.buildings.length) container.append(element("p", "empty-state", "Create the first building above."));
}

async function saveBuilding(event) {
  event.preventDefault();
  const form = event.currentTarget;
  if (!form.reportValidity()) return;
  const id = $("building-id").value;
  const body = { name: $("building-name").value, sortOrder: Number($("building-order").value || 0) };
  const button = form.querySelector("button[type=submit]");
  await runButton(button, async () => {
    await request(id ? `/api/portal/admin/buildings/${encodeURIComponent(id)}` : "/api/portal/admin/buildings", {
      method: id ? "PUT" : "POST",
      body
    });
    clearBuildingForm();
    await loadMap();
    showGlobalMessage("Building saved.");
  });
}

function editBuilding(building) {
  $("building-id").value = building.id;
  $("building-name").value = building.name;
  $("building-order").value = building.sortOrder;
  $("building-name").focus();
}

function clearBuildingForm() {
  $("building-form").reset();
  $("building-id").value = "";
  $("building-order").value = "0";
}

async function saveFloor(event) {
  event.preventDefault();
  const form = event.currentTarget;
  if (!form.reportValidity()) return;
  const id = $("floor-id").value;
  const body = {
    buildingId: $("floor-building").value,
    name: $("floor-name-input").value,
    sortOrder: Number($("floor-order").value || 0)
  };
  const button = form.querySelector("button[type=submit]");
  await runButton(button, async () => {
    await request(id ? `/api/portal/admin/floors/${encodeURIComponent(id)}` : "/api/portal/admin/floors", {
      method: id ? "PUT" : "POST",
      body
    });
    clearFloorForm();
    await loadMap();
    showGlobalMessage("Floor saved.");
  });
}

function editFloor(floor) {
  $("floor-id").value = floor.id;
  $("floor-building").value = floor.buildingId;
  $("floor-name-input").value = floor.name;
  $("floor-order").value = floor.sortOrder;
  $("floor-name-input").focus();
}

function clearFloorForm() {
  $("floor-form").reset();
  $("floor-id").value = "";
  $("floor-order").value = "0";
  fillBuildingSelect($("floor-building"));
}

function fillBuildingSelect(select) {
  const selected = select.value;
  select.replaceChildren();
  app.map?.buildings.forEach((building) => {
    const option = element("option", "", building.name);
    option.value = building.id;
    select.append(option);
  });
  if ([...select.options].some((option) => option.value === selected)) select.value = selected;
}

function renderMapAdministration() {
  if (!app.map) return;
  updatePlanScopeOptions();
  const select = $("editor-plan-select");
  const selected = app.editor.key || select.value;
  select.replaceChildren();
  allPlanOptions().forEach((entry) => {
    const option = element("option", "", entry.label);
    option.value = entry.key;
    option.disabled = !entry.plan;
    select.append(option);
  });
  const validSelected = allPlanOptions().find((entry) => entry.key === selected && entry.plan);
  const first = allPlanOptions().find((entry) => entry.plan);
  const next = validSelected?.key || first?.key || "";
  select.value = next;
  if (next && next !== app.editor.key) selectEditorPlan(next);
  else if (!next) {
    app.editor.key = null;
    app.editor.plan = null;
    renderEditor();
  } else {
    renderEditor();
  }
}

function allPlanOptions() {
  const output = [];
  (app.map?.buildings || []).forEach((building) => {
    output.push({ key: `building:${building.id}`, label: `${building.name} · overview`, plan: building.overviewPlan, regions: [] });
    building.floors.forEach((floor) => {
      output.push({ key: `floor:${floor.id}`, label: `${building.name} · ${floor.name}`, plan: floor.floorPlan, regions: floor.regions });
    });
  });
  return output;
}

function updatePlanScopeOptions() {
  const type = $("plan-scope-type").value;
  const select = $("plan-scope-id");
  const selected = select.value;
  select.replaceChildren();
  if (type === "building") {
    app.map?.buildings.forEach((building) => {
      const option = element("option", "", building.name);
      option.value = building.id;
      select.append(option);
    });
  } else {
    app.map?.buildings.forEach((building) => {
      building.floors.forEach((floor) => {
        const option = element("option", "", `${building.name} · ${floor.name}`);
        option.value = floor.id;
        select.append(option);
      });
    });
  }
  if ([...select.options].some((option) => option.value === selected)) select.value = selected;
}

function updateFileLabel() {
  const file = $("plan-pdf").files[0];
  setText("plan-file-label", file?.name || "Choose a PDF");
}

async function uploadFloorPlan(event) {
  event.preventDefault();
  const form = event.currentTarget;
  if (!form.reportValidity()) return;
  const button = $("plan-upload-button");
  const data = new FormData();
  data.append("scopeType", $("plan-scope-type").value);
  data.append("scopeId", $("plan-scope-id").value);
  data.append("name", $("plan-name").value);
  data.append("pdf", $("plan-pdf").files[0]);
  showFormMessage("plan-upload-message", "Rendering the first PDF page as a background…");
  await runButton(button, async () => {
    try {
      await request("/api/portal/admin/floorplans", { method: "POST", body: data });
      form.reset();
      updateFileLabel();
      await loadMap();
      showFormMessage("plan-upload-message", "PDF background uploaded. You can now draw service zones.");
    } catch (error) {
      showFormMessage("plan-upload-message", error.message, true);
    }
  });
}

function selectEditorPlan(key) {
  const entry = allPlanOptions().find((item) => item.key === key && item.plan);
  app.editor.key = entry?.key || null;
  app.editor.plan = entry?.plan || null;
  app.editor.trace = JSON.parse(JSON.stringify(entry?.plan?.trace || []));
  app.editor.regions = JSON.parse(JSON.stringify(entry?.regions || []));
  app.editor.tool = "select";
  app.editor.selection = null;
  app.editor.pendingPoints = [];
  app.editor.draftRegion = null;
  app.editor.drag = null;
  syncToolButtons();
  cancelRegionForm();
  renderEditor();
}

function renderEditor() {
  const container = $("map-editor");
  container.replaceChildren();
  const plan = app.editor.plan;
  setText("editor-plan-name", plan?.name || "Choose a map to edit");
  if (!plan) {
    renderMapEmpty(container, "Upload a building overview or floor PDF to begin editing.");
    $("save-trace-button").disabled = true;
    return;
  }
  $("save-trace-button").disabled = false;
  const svg = svgElement("svg");
  svg.setAttribute("viewBox", `0 0 ${plan.width} ${plan.height}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  const image = svgElement("image", "plan-image");
  if ($("show-original-plan").checked) image.classList.add("visible-original");
  image.setAttribute("href", plan.imageUrl);
  image.setAttribute("width", String(plan.width));
  image.setAttribute("height", String(plan.height));
  svg.append(image);
  app.editor.trace.forEach((feature) => {
    const node = traceNode(feature, plan);
    node.dataset.editorType = "feature";
    if (app.editor.selection?.type === "feature" && app.editor.selection.id === feature.id) node.classList.add("selected");
    node.addEventListener("pointerdown", (event) => {
      event.stopPropagation();
      selectEditorFeature(feature.id);
    });
    svg.append(node);
  });
  app.editor.regions.forEach((region) => {
    const node = regionNode(region, plan);
    node.dataset.editorType = "region";
    if (app.editor.selection?.type === "region" && app.editor.selection.id === region.id) node.classList.add("selected");
    node.addEventListener("pointerdown", (event) => {
      event.stopPropagation();
      selectEditorRegion(region.id);
    });
    svg.append(node);
  });
  if (app.editor.pendingPoints.length) {
    const preview = svgElement(app.editor.tool === "region" ? "polygon" : "polyline", "drawing-preview");
    preview.setAttribute("points", pointsAttribute(app.editor.pendingPoints, plan));
    svg.append(preview);
  }
  renderEditorHandles(svg);
  svg.addEventListener("pointerdown", editorCanvasPointerDown);
  svg.addEventListener("pointermove", editorCanvasPointerMove);
  svg.addEventListener("pointerup", editorCanvasPointerUp);
  svg.addEventListener("pointercancel", editorCanvasPointerUp);
  container.append(svg);
  updateEditorSelectionLabel();
}

function setEditorTool(tool) {
  if (tool === "region" && !app.editor.key?.startsWith("floor:")) {
    showGlobalMessage("Service regions can only be drawn on a floor plan.", true);
    return;
  }
  if (app.editor.selection?.type === "feature" && ["wall", "door", "cubicle", "furniture"].includes(tool)) {
    const feature = app.editor.trace.find((item) => item.id === app.editor.selection.id);
    if (feature) feature.kind = tool;
    app.editor.tool = "select";
  } else {
    app.editor.tool = tool;
  }
  app.editor.pendingPoints = [];
  $("finish-region-button").hidden = true;
  syncToolButtons();
  renderEditor();
}

function syncToolButtons() {
  document.querySelectorAll(".tool-button").forEach((button) => {
    button.disabled = button.dataset.tool === "region" && !app.editor.key?.startsWith("floor:");
    button.classList.toggle("active", button.dataset.tool === app.editor.tool);
  });
  const hints = {
    select: "Select a zone and drag its points to adjust the boundary.",
    wall: "Click two points to draw a wall.",
    door: "Click two points to draw a door.",
    cubicle: "Click two points to draw a cubicle partition.",
    furniture: "Click successive endpoints to add a furniture edge.",
    region: "Click around the service area, then finish the boundary."
  };
  setText("editor-hint", hints[app.editor.tool]);
}

function editorCanvasPointerDown(event) {
  if (!app.editor.plan || event.button !== 0) return;
  const svg = event.currentTarget;
  const point = normalizedPointer(event, svg, app.editor.plan);
  if (app.editor.tool === "select") {
    app.editor.selection = null;
    renderEditor();
    return;
  }
  if (["wall", "door", "cubicle", "furniture"].includes(app.editor.tool)) {
    app.editor.pendingPoints.push(point);
    if (app.editor.pendingPoints.length === 2) {
      app.editor.trace.push({
        id: crypto.randomUUID(),
        kind: app.editor.tool,
        points: [...app.editor.pendingPoints],
        thickness: app.editor.tool === "wall" ? 3 : 1.5
      });
      app.editor.pendingPoints = [];
    }
    renderEditor();
    return;
  }
  if (app.editor.tool === "region") {
    app.editor.pendingPoints.push(point);
    $("finish-region-button").hidden = app.editor.pendingPoints.length < 3;
    renderEditor();
  }
}

function renderEditorHandles(svg) {
  const selection = app.editor.selection;
  if (!selection) return;
  const points = selection.type === "feature"
    ? app.editor.trace.find((item) => item.id === selection.id)?.points
    : app.editor.regions.find((item) => item.id === selection.id)?.polygon;
  (points || []).forEach((point, index) => {
    const circle = svgElement("circle", "editor-handle");
    circle.setAttribute("cx", String(point.x * app.editor.plan.width));
    circle.setAttribute("cy", String(point.y * app.editor.plan.height));
    circle.setAttribute("r", String(Math.max(4, app.editor.plan.width / 180)));
    circle.dataset.pointIndex = String(index);
    circle.addEventListener("pointerdown", (event) => {
      event.stopPropagation();
      circle.setPointerCapture(event.pointerId);
      app.editor.drag = { selection: { ...selection }, index, pointerId: event.pointerId };
    });
    svg.append(circle);
  });
}

function editorCanvasPointerMove(event) {
  if (!app.editor.drag) return;
  const point = normalizedPointer(event, event.currentTarget, app.editor.plan);
  const { selection, index } = app.editor.drag;
  const selected = selection.type === "feature"
    ? app.editor.trace.find((item) => item.id === selection.id)
    : app.editor.regions.find((item) => item.id === selection.id);
  const points = selection.type === "feature" ? selected?.points : selected?.polygon;
  if (!points?.[index]) return;
  points[index] = point;

  const svg = event.currentTarget;
  const shape = [...svg.querySelectorAll(`[data-editor-type="${selection.type}"]`)]
    .find((node) => node.dataset.id === selection.id);
  shape?.setAttribute("points", pointsAttribute(points, app.editor.plan));
  const handle = [...svg.querySelectorAll(".editor-handle")]
    .find((node) => Number(node.dataset.pointIndex) === index);
  handle?.setAttribute("cx", String(point.x * app.editor.plan.width));
  handle?.setAttribute("cy", String(point.y * app.editor.plan.height));
}

function editorCanvasPointerUp() {
  app.editor.drag = null;
}

function normalizedPointer(event, svg, plan) {
  const point = svg.createSVGPoint();
  point.x = event.clientX;
  point.y = event.clientY;
  const local = point.matrixTransform(svg.getScreenCTM().inverse());
  return {
    x: clamp(local.x / plan.width, 0, 1),
    y: clamp(local.y / plan.height, 0, 1)
  };
}

function selectEditorFeature(id) {
  app.editor.selection = { type: "feature", id };
  app.editor.tool = "select";
  syncToolButtons();
  $("region-form").hidden = true;
  renderEditor();
}

function selectEditorRegion(id) {
  const region = app.editor.regions.find((item) => item.id === id);
  if (!region) return;
  app.editor.selection = { type: "region", id };
  app.editor.tool = "select";
  syncToolButtons();
  populateRegionForm(region);
  renderEditor();
}

function finishRegionBoundary() {
  if (app.editor.pendingPoints.length < 3) return;
  app.editor.draftRegion = {
    id: null,
    floorId: app.editor.plan.scopeId,
    name: "",
    color: "#2cc7d2",
    polygon: [...app.editor.pendingPoints],
    favBox: "",
    temperatureMapping: { objectId: "", attributeId: "85" }
  };
  app.editor.pendingPoints = [];
  app.editor.tool = "select";
  $("finish-region-button").hidden = true;
  syncToolButtons();
  populateRegionForm(app.editor.draftRegion);
  renderEditor();
}

function populateRegionForm(region) {
  $("region-form").hidden = false;
  $("region-id").value = region.id || "";
  setText("region-form-title", region.id ? "Edit region" : "New region");
  $("region-name").value = region.name || "";
  $("region-color").value = region.color || "#2cc7d2";
  $("region-fav").value = region.favBox || "";
  $("region-object-id").value = region.temperatureMapping?.objectId || "";
  $("region-attribute-id").value = region.temperatureMapping?.attributeId || "85";
  $("region-name").focus();
}

async function saveRegion(event) {
  event.preventDefault();
  const form = event.currentTarget;
  if (!form.reportValidity() || !app.editor.plan || !app.editor.key.startsWith("floor:")) return;
  const id = $("region-id").value;
  const existing = id ? app.editor.regions.find((item) => item.id === id) : app.editor.draftRegion;
  if (!existing?.polygon?.length) return;
  const body = {
    floorId: app.editor.plan.scopeId,
    name: $("region-name").value,
    color: $("region-color").value,
    polygon: existing.polygon,
    favBox: $("region-fav").value,
    metasysObjectId: $("region-object-id").value,
    metasysAttributeId: $("region-attribute-id").value
  };
  const button = form.querySelector("button[type=submit]");
  await runButton(button, async () => {
    await request(id ? `/api/portal/admin/regions/${encodeURIComponent(id)}` : "/api/portal/admin/regions", {
      method: id ? "PUT" : "POST",
      body
    });
    app.editor.draftRegion = null;
    cancelRegionForm();
    const key = app.editor.key;
    await loadMap();
    selectEditorPlan(key);
    showGlobalMessage("Service area saved.");
  });
}

function cancelRegionForm() {
  $("region-form").hidden = true;
  $("region-form").reset();
  $("region-id").value = "";
  $("region-attribute-id").value = "85";
  app.editor.draftRegion = null;
}

async function deleteEditorSelection() {
  const selection = app.editor.selection;
  if (!selection) return;
  if (selection.type === "feature") {
    app.editor.trace = app.editor.trace.filter((feature) => feature.id !== selection.id);
    app.editor.selection = null;
    renderEditor();
    return;
  }
  if (!window.confirm("Delete this region? Existing service requests prevent deletion.")) return;
  try {
    await request(`/api/portal/admin/regions/${encodeURIComponent(selection.id)}`, { method: "DELETE" });
    const key = app.editor.key;
    await loadMap();
    selectEditorPlan(key);
    showGlobalMessage("Region deleted.");
  } catch (error) {
    showGlobalMessage(error.message, true);
  }
}

async function saveTrace() {
  if (!app.editor.plan) return;
  const button = $("save-trace-button");
  await runButton(button, async () => {
    await request(`/api/portal/admin/floorplans/${encodeURIComponent(app.editor.plan.id)}/trace`, {
      method: "PUT",
      body: { trace: app.editor.trace }
    });
    const key = app.editor.key;
    await loadMap();
    selectEditorPlan(key);
    showGlobalMessage("Drawing lines saved.");
  });
}

function updateEditorSelectionLabel() {
  const selection = app.editor.selection;
  let label = "No selection";
  if (selection?.type === "feature") {
    const feature = app.editor.trace.find((item) => item.id === selection.id);
    if (feature) label = `${feature.kind} · ${feature.points.length} points`;
  }
  if (selection?.type === "region") {
    const region = app.editor.regions.find((item) => item.id === selection.id);
    if (region) label = `Region · ${region.name}`;
  }
  setText("editor-selection-label", label);
  $("delete-selection-button").disabled = !selection;
}

async function loadUsers() {
  try {
    app.users = await request("/api/portal/admin/users");
    renderUsers();
  } catch (error) {
    showGlobalMessage(error.message || "Unable to load users", true);
  }
}

function renderUsers() {
  if (app.session?.user.role !== "admin" || !app.map) return;
  renderScopeList();
  const list = $("user-list");
  list.replaceChildren();
  app.users.forEach((user) => {
    const card = element("article", "user-card");
    const info = element("div");
    info.append(element("strong", "", user.displayName));
    info.append(element("p", "", user.email));
    info.append(element("span", `role-badge${user.active ? "" : " inactive"}`, user.active ? roleLabel(user.role) : "Inactive"));
    const edit = element("button", "secondary-button", "Edit");
    edit.type = "button";
    edit.addEventListener("click", () => editUser(user));
    card.append(info, edit);
    list.append(card);
  });
  updateUserScopeVisibility();
}

function renderScopeList(selectedFloors = [], selectedRegions = []) {
  const container = $("scope-list");
  container.replaceChildren();
  app.map.buildings.forEach((building) => {
    const group = element("div", "scope-building");
    group.append(element("strong", "", building.name));
    building.floors.forEach((floor) => {
      const row = element("div", "scope-row");
      const floorLabel = element("label");
      const floorCheck = element("input");
      floorCheck.type = "checkbox";
      floorCheck.name = "scope-floor";
      floorCheck.value = floor.id;
      floorCheck.checked = selectedFloors.includes(floor.id);
      floorLabel.append(floorCheck, document.createTextNode(`Entire ${floor.name}`));
      row.append(floorLabel);
      const regions = element("div", "scope-regions");
      floor.regions.forEach((region) => {
        const regionLabel = element("label");
        const regionCheck = element("input");
        regionCheck.type = "checkbox";
        regionCheck.name = "scope-region";
        regionCheck.value = region.id;
        regionCheck.checked = selectedRegions.includes(region.id);
        regionLabel.append(regionCheck, document.createTextNode(region.name));
        regions.append(regionLabel);
      });
      floorCheck.addEventListener("change", () => {
        regions.querySelectorAll("input").forEach((input) => {
          input.disabled = floorCheck.checked;
          if (floorCheck.checked) input.checked = false;
        });
      });
      if (floorCheck.checked) regions.querySelectorAll("input").forEach((input) => { input.disabled = true; });
      row.append(regions);
      group.append(row);
    });
    container.append(group);
  });
}

function editUser(user) {
  $("user-id").value = user.id;
  setText("user-form-title", "Edit user");
  $("user-display-name").value = user.displayName;
  $("user-email").value = user.email;
  $("user-role").value = user.role;
  $("user-password").value = "";
  $("user-active").checked = user.active;
  setText("user-password-help", "Leave blank to keep the current password.");
  renderScopeList(user.floorIds, user.regionIds);
  updateUserScopeVisibility();
  $("user-display-name").focus();
}

function clearUserForm() {
  $("user-form").reset();
  $("user-id").value = "";
  setText("user-form-title", "Create user");
  setText("user-password-help", "Required for a new account; 12 characters minimum.");
  $("user-active").checked = true;
  $("user-role").value = "viewOnly";
  renderScopeList();
  updateUserScopeVisibility();
  showFormMessage("user-form-message", "");
}

function updateUserScopeVisibility() {
  const role = $("user-role").value;
  $("user-scopes").hidden = role === "admin" || role === "operator";
}

async function saveUser(event) {
  event.preventDefault();
  const form = event.currentTarget;
  if (!form.reportValidity()) return;
  const id = $("user-id").value;
  const password = $("user-password").value;
  if (!id && password.length < 12) {
    showFormMessage("user-form-message", "A new account needs a password of at least 12 characters.", true);
    return;
  }
  const floorIds = [...document.querySelectorAll('input[name="scope-floor"]:checked')].map((input) => input.value);
  const regionIds = [...document.querySelectorAll('input[name="scope-region"]:checked:not(:disabled)')].map((input) => input.value);
  const body = {
    displayName: $("user-display-name").value,
    email: $("user-email").value,
    role: $("user-role").value,
    password: password || (id ? null : ""),
    active: $("user-active").checked,
    floorIds,
    regionIds
  };
  const button = form.querySelector("button[type=submit]");
  await runButton(button, async () => {
    try {
      await request(id ? `/api/portal/admin/users/${encodeURIComponent(id)}` : "/api/portal/admin/users", {
        method: id ? "PUT" : "POST",
        body
      });
      clearUserForm();
      await loadUsers();
      showGlobalMessage("User account saved.");
    } catch (error) {
      showFormMessage("user-form-message", error.message, true);
    }
  });
}

async function request(path, options = {}) {
  const headers = new Headers(options.headers || {});
  const method = (options.method || "GET").toUpperCase();
  let body = options.body;
  if (body !== undefined && body !== null && !(body instanceof FormData)) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(body);
  }
  if (!isSafeMethod(method) && app.session?.csrfToken) {
    headers.set("X-CSRF-Token", app.session.csrfToken);
  }
  const response = await window.fetch(path, {
    method,
    headers,
    body,
    cache: options.cache || "no-store",
    credentials: "same-origin"
  });
  if (!response.ok) {
    let message = `Request failed (${response.status})`;
    try {
      const error = await response.json();
      if (error.error) message = error.error;
    } catch (_) {
      // Keep the status fallback for non-JSON failures.
    }
    const failure = new Error(message);
    failure.status = response.status;
    if (response.status === 401 && !options.allowUnauthorized && app.session) {
      window.setTimeout(() => window.location.assign("/"), 600);
    }
    throw failure;
  }
  if (response.status === 204) return null;
  const contentType = response.headers.get("content-type") || "";
  return contentType.includes("application/json") ? response.json() : response.text();
}

function isSafeMethod(method) {
  return method === "GET" || method === "HEAD" || method === "OPTIONS";
}

async function runButton(button, operation) {
  button.disabled = true;
  try {
    await operation();
  } catch (error) {
    showGlobalMessage(error.message || "Request failed", true);
  } finally {
    button.disabled = false;
  }
}

function showLoginMessage(message, isError = false) {
  showFormMessage("login-message", message, isError);
}

function showFormMessage(id, message, isError = false) {
  const node = $(id);
  node.textContent = message || "";
  node.classList.toggle("error", isError);
}

let globalMessageTimer;
function showGlobalMessage(message, isError = false) {
  const node = $("global-message");
  window.clearTimeout(globalMessageTimer);
  node.textContent = message;
  node.classList.toggle("error", isError);
  node.hidden = false;
  globalMessageTimer = window.setTimeout(() => { node.hidden = true; }, 6000);
}

function detailItem(label, value) {
  const item = element("div", "detail-item");
  item.append(element("span", "", label), element("strong", "", value));
  return item;
}

function makeButton(label, active) {
  const button = element("button", active ? "active" : "", label);
  button.type = "button";
  return button;
}

function element(tag, className = "", text = null) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== null) node.textContent = text;
  return node;
}

function svgElement(tag, className = "") {
  const node = document.createElementNS(SVG_NS, tag);
  if (className) node.setAttribute("class", className);
  return node;
}

function setText(id, value) {
  $(id).textContent = value;
}

function clamp(value, minimum, maximum) {
  return Math.min(maximum, Math.max(minimum, value));
}

function roleLabel(role) {
  return ({ admin: "Administrator", viewOnly: "View only", operator: "Operator", reportingStaff: "Reporting staff" })[role] || role;
}

function issueLabel(issue) {
  return ({ too_hot: "Too hot", too_cold: "Too cold", lighting: "Lighting", water_leak: "Water leak", noise: "Noise", broken_toilet: "Broken toilet", air_quality: "Air quality", other: "Other" })[issue] || issue;
}

function statusLabel(status) {
  return ({ open: "Open", inProgress: "In progress", resolved: "Resolved", closed: "Closed" })[status] || status;
}

function formatTime(value) {
  if (!value) return "Unknown time";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(value));
}

function relativeTime(value) {
  if (!value) return "unknown";
  const seconds = Math.round((new Date(value).getTime() - Date.now()) / 1000);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (Math.abs(seconds) < 60) return formatter.format(seconds, "second");
  const minutes = Math.round(seconds / 60);
  if (Math.abs(minutes) < 60) return formatter.format(minutes, "minute");
  const hours = Math.round(minutes / 60);
  if (Math.abs(hours) < 24) return formatter.format(hours, "hour");
  return formatter.format(Math.round(hours / 24), "day");
}
