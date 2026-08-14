"use strict";

const state = {
  session: null,
  inventory: null,
  selected: null,
  selectedGroup: "",
  liveValues: new Map(),
  liveGeneratedAt: null,
  liveError: "",
  liveLoading: false,
  liveRequestVersion: 0,
  liveRequestInFlight: null,
  liveTimer: null,
  refreshIntervalMilliseconds: 1000,
  refreshReason: "oneSecondTarget",
  queryDurationMilliseconds: null,
  consecutiveFailures: 0
};

const $ = (id) => document.getElementById(id);

document.addEventListener("DOMContentLoaded", async () => {
  $("equipment-search").addEventListener("input", renderTree);
  $("point-search").addEventListener("input", renderPoints);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && state.selected) scheduleLiveRefresh(0);
  });
  try {
    const session = await fetchJson("/api/portal/me");
    if (!["admin", "operator"].includes(session.user.role)) {
      window.location.assign("/");
      return;
    }
    state.session = session;
    window.MetasysNavigation?.configure(session);
    window.MetasysNavigation?.setActive("equipment");
    state.inventory = await fetchJson("/api/equipment-inventory");
    if (!state.inventory) {
      showMessage("No equipment inventory has been imported yet.", true);
      renderEmpty();
      return;
    }
    renderSummary();
    renderTree();
    const firstGroup = state.inventory.groups?.find((group) => group.equipment?.length);
    if (firstGroup) selectEquipment(firstGroup.equipment[0], firstGroup.name);
  } catch (error) {
    showMessage(error.message || "Unable to load the equipment hierarchy", true);
    renderEmpty();
  }
});

function renderSummary() {
  const inventory = state.inventory;
  const equipment = allEquipment();
  const pointCount = equipment.reduce((total, item) => total + (item.points?.length || 0), 0);
  const fanCount = equipment.filter((item) => item.variant === "fanPoweredHeating").length;
  const coolingCount = equipment.filter((item) => item.variant === "coolingOnly").length;
  $("inventory-title").textContent = inventory.rootName || "Discovered equipment and points";
  $("inventory-source").textContent = inventory.sourceSummary || "Imported discovery inventory";
  $("captured-at").textContent = formatDate(inventory.capturedAt);
  $("inventory-status").textContent = `${inventory.groups?.length || 0} hierarchy groups imported`;
  $("equipment-count").textContent = formatInteger(equipment.length);
  $("point-count").textContent = formatInteger(pointCount);
  $("fan-count").textContent = formatInteger(fanCount);
  $("cooling-count").textContent = formatInteger(coolingCount);
  $("group-count").textContent = `${inventory.groups?.length || 0} groups`;
  renderNotes();
}

function renderTree() {
  if (!state.inventory) return;
  const query = $("equipment-search").value.trim().toLocaleLowerCase();
  const tree = $("equipment-tree");
  const fragment = document.createDocumentFragment();
  const root = element("div", "tree-root");
  root.append(element("span", "tree-folder-icon", "◆"), element("strong", "", state.inventory.rootName || "Equipment"));
  fragment.append(root);

  let matches = 0;
  for (const group of state.inventory.groups || []) {
    const equipment = (group.equipment || []).filter((item) => equipmentMatches(item, group, query));
    if (!equipment.length && query) continue;
    matches += equipment.length;
    const branch = document.createElement("details");
    branch.className = "tree-group";
    branch.open = true;
    const summary = document.createElement("summary");
    summary.append(
      element("span", "tree-folder-icon", "▦"),
      element("strong", "", group.name),
      element("small", "", String(equipment.length))
    );
    branch.append(summary);
    const items = element("div", "tree-items");
    for (const item of equipment) {
      const button = element("button", `tree-equipment${state.selected === item ? " selected" : ""}`);
      button.type = "button";
      button.append(
        element("span", `equipment-dot ${variantClass(item.variant)}`, ""),
        element("span", "tree-equipment-copy", ""),
        element("small", "tree-point-count", String(item.points?.length || 0))
      );
      const copy = button.querySelector(".tree-equipment-copy");
      copy.append(element("strong", "", item.name), element("small", "", treeSubtitle(item)));
      button.addEventListener("click", () => selectEquipment(item, group.name));
      items.append(button);
    }
    branch.append(items);
    fragment.append(branch);
  }
  if (!matches) fragment.append(element("p", "empty-state", "No equipment or points match this filter."));
  tree.replaceChildren(fragment);
}

function selectEquipment(equipment, groupName) {
  state.liveRequestVersion += 1;
  clearTimeout(state.liveTimer);
  state.selected = equipment;
  state.selectedGroup = groupName;
  state.liveValues = new Map();
  state.liveGeneratedAt = null;
  state.liveError = "";
  state.liveLoading = livePointIds(equipment).length > 0;
  state.liveRequestInFlight = null;
  state.refreshIntervalMilliseconds = 1000;
  state.refreshReason = "oneSecondTarget";
  state.queryDurationMilliseconds = null;
  state.consecutiveFailures = 0;
  $("point-search").value = "";
  $("detail-group").textContent = groupName;
  $("detail-title").textContent = equipment.name;
  $("detail-variant").textContent = `${variantLabel(equipment.variant)} · ${equipment.points?.length || 0} selected points`;
  $("detail-status").textContent = equipment.discoveryStatus || "Imported";
  $("detail-status").className = `status-pill ${statusClass(equipment.discoveryStatus)}`;
  $("equipment-metadata").hidden = false;
  $("meta-protocol").textContent = equipment.protocol || "—";
  $("meta-network").textContent = equipment.networkName || "—";
  $("meta-mac").textContent = equipment.macAddress ?? "Not resolved";
  $("meta-instance").textContent = equipment.deviceInstance == null ? "Not resolved" : formatInteger(equipment.deviceInstance);
  $("meta-reference").textContent = equipment.objectReference || "Not available";
  renderTree();
  renderPoints();
  scheduleLiveRefresh(0);
}

function renderPoints() {
  const equipment = state.selected;
  const body = $("point-table-body");
  if (!equipment) {
    body.replaceChildren(tableMessage("No equipment selected."));
    return;
  }
  const query = $("point-search").value.trim().toLocaleLowerCase();
  const points = (equipment.points || []).filter((point) => {
    if (!query) return true;
    return `${point.name} ${point.category} ${point.unit || ""} ${point.reference} ${point.source}`.toLocaleLowerCase().includes(query);
  });
  const historianCount = (equipment.points || []).filter((point) => point.historianPointSliceId != null).length;
  $("detail-point-count").textContent = `${formatInteger(equipment.points?.length || 0)} points`;
  $("detail-historian-count").textContent = `${formatInteger(historianCount)} historian-backed`;
  renderLiveStatus(historianCount);
  const fragment = document.createDocumentFragment();
  for (const point of points) {
    const row = document.createElement("tr");
    const nameCell = document.createElement("td");
    nameCell.append(element("strong", "", point.name));
    if (point.reference) nameCell.append(element("code", "", point.reference));
    const valueCell = renderValueCell(point);
    const updatedCell = renderUpdatedCell(point);
    const categoryCell = document.createElement("td");
    categoryCell.append(element("span", `point-category ${categoryClass(point.category)}`, categoryLabel(point.category)));
    const sample = state.liveValues.get(point.historianPointSliceId);
    const unitCell = element("td", "", point.unit || sample?.unit || "—");
    const sourceCell = document.createElement("td");
    sourceCell.append(element("span", point.historianPointSliceId == null ? "source-badge" : "source-badge historian", point.historianPointSliceId == null ? point.source : `Historian · ${point.historianPointSliceId}`));
    row.append(nameCell, valueCell, updatedCell, categoryCell, unitCell, sourceCell);
    fragment.append(row);
  }
  if (!points.length) fragment.append(tableMessage("No points match this filter."));
  body.replaceChildren(fragment);
}

function renderValueCell(point) {
  const cell = document.createElement("td");
  cell.className = "live-value-cell";
  if (point.historianPointSliceId == null) {
    cell.append(element("span", "live-value unavailable", "Not historized"));
    return cell;
  }
  const sample = state.liveValues.get(point.historianPointSliceId);
  if (sample) {
    const value = element("strong", "live-value", formatPointValue(sample.value, point.category));
    value.title = `Historian value ${sample.value}`;
    cell.append(value);
  } else if (state.liveLoading && !state.liveGeneratedAt) {
    cell.append(element("span", "live-value pending", "Loading…"));
  } else {
    cell.append(element("span", "live-value unavailable", "No recent sample"));
  }
  return cell;
}

function renderUpdatedCell(point) {
  const cell = document.createElement("td");
  cell.className = "sample-time-cell";
  if (point.historianPointSliceId == null) {
    cell.textContent = "—";
    return cell;
  }
  const sample = state.liveValues.get(point.historianPointSliceId);
  if (!sample) {
    cell.textContent = state.liveLoading && !state.liveGeneratedAt ? "Checking…" : "—";
    return cell;
  }
  const relative = element("span", "", formatRelativeTime(sample.timestamp));
  relative.title = formatDate(sample.timestamp);
  const exact = element("small", "", formatTime(sample.timestamp));
  exact.title = formatDate(sample.timestamp);
  cell.append(relative, exact);
  return cell;
}

function livePointIds(equipment = state.selected) {
  return [...new Set((equipment?.points || [])
    .map((point) => point.historianPointSliceId)
    .filter((value) => Number.isInteger(value) && value > 0))];
}

function scheduleLiveRefresh(delayMilliseconds) {
  clearTimeout(state.liveTimer);
  if (!state.selected || !livePointIds().length) return;
  state.liveTimer = window.setTimeout(refreshLiveValues, Math.max(0, delayMilliseconds));
}

async function refreshLiveValues() {
  const equipment = state.selected;
  const pointIds = livePointIds(equipment);
  if (!equipment || !pointIds.length) return;
  if (document.hidden) {
    scheduleLiveRefresh(state.refreshIntervalMilliseconds);
    return;
  }

  const requestVersion = state.liveRequestVersion;
  if (state.liveRequestInFlight === requestVersion) return;
  state.liveRequestInFlight = requestVersion;
  const startedAt = Date.now();
  state.liveLoading = true;
  renderLiveStatus(pointIds.length);
  try {
    const query = new URLSearchParams({ pointSlices: pointIds.join(",") });
    const response = await fetchJson(`/api/equipment-values?${query}`);
    if (requestVersion !== state.liveRequestVersion || equipment !== state.selected) return;
    const refreshMilliseconds = Number(
      response.refreshIntervalMilliseconds ?? Number(response.refreshIntervalSeconds) * 1000
    );
    if (Number.isFinite(refreshMilliseconds) && refreshMilliseconds >= 1000 && refreshMilliseconds <= 60000) {
      state.refreshIntervalMilliseconds = refreshMilliseconds;
    }
    state.refreshReason = response.refreshReason || "oneSecondTarget";
    const queryDuration = Number(response.queryDurationMilliseconds);
    state.queryDurationMilliseconds = Number.isFinite(queryDuration) ? queryDuration : null;
    state.consecutiveFailures = 0;
    state.liveValues = new Map((response.values || []).map((sample) => [sample.pointSliceId, sample]));
    state.liveGeneratedAt = response.generatedAt || new Date().toISOString();
    state.liveError = "";
  } catch (error) {
    if (requestVersion !== state.liveRequestVersion || equipment !== state.selected) return;
    state.liveError = error.message || "Unable to refresh historian values";
    state.consecutiveFailures += 1;
    state.refreshIntervalMilliseconds = Math.min(
      60000,
      Math.max(1000, state.refreshIntervalMilliseconds * 2)
    );
    state.refreshReason = "errorBackoff";
  } finally {
    if (state.liveRequestInFlight === requestVersion) state.liveRequestInFlight = null;
    if (requestVersion === state.liveRequestVersion && equipment === state.selected) {
      state.liveLoading = false;
      renderPoints();
      const elapsed = Date.now() - startedAt;
      const jitteredInterval = state.refreshIntervalMilliseconds * (1 + Math.random() * 0.1);
      scheduleLiveRefresh(Math.max(100, jitteredInterval - elapsed));
    }
  }
}

function renderLiveStatus(historianCount = livePointIds().length) {
  const node = $("live-refresh-status");
  node.className = "live-refresh-status";
  if (!historianCount) {
    node.textContent = "No historian values are linked to this equipment";
    return;
  }
  if (state.liveError) {
    node.classList.add("error");
    node.textContent = `Historian refresh failed · retrying in ${formatRefreshInterval(state.refreshIntervalMilliseconds)}`;
    node.title = state.liveError;
    return;
  }
  if (state.liveLoading && !state.liveGeneratedAt) {
    node.textContent = "Loading latest historian values · 1-second target";
    return;
  }
  const checked = state.liveGeneratedAt ? ` · checked ${formatRelativeTime(state.liveGeneratedAt)}` : "";
  const reason = refreshReasonLabel(state.refreshReason);
  node.textContent = `Historian refresh every ${formatRefreshInterval(state.refreshIntervalMilliseconds)} · ${reason}${checked}${state.liveLoading ? " · refreshing" : ""}`;
  const queryDuration = state.queryDurationMilliseconds == null ? "unknown" : `${state.queryDurationMilliseconds} ms`;
  node.title = `Read-only SQL historian query (${historianCount} points, last query ${queryDuration}). This page does not poll the BACnet MS/TP trunk.`;
}

function formatRefreshInterval(milliseconds) {
  const seconds = Math.max(1, Number(milliseconds) / 1000);
  return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(seconds)} ${seconds === 1 ? "second" : "seconds"}`;
}

function refreshReasonLabel(reason) {
  const labels = {
    oneSecondTarget: "1-second target",
    pointCount: "adjusted for point count",
    queryLatency: "adjusted for query latency",
    safetyCap: "60-second safety cap",
    errorBackoff: "temporary failure backoff"
  };
  return labels[reason] || "adaptive rate";
}

function renderNotes() {
  const notes = state.inventory?.notes || [];
  const footer = $("inventory-notes");
  if (!notes.length) {
    footer.hidden = true;
    return;
  }
  footer.hidden = false;
  footer.replaceChildren(element("strong", "", "Discovery notes"));
  const list = document.createElement("ul");
  for (const note of notes) list.append(element("li", "", note));
  footer.append(list);
}

function renderEmpty() {
  $("equipment-tree").replaceChildren(element("p", "empty-state", "No imported hierarchy is available."));
  $("point-table-body").replaceChildren(tableMessage("No imported hierarchy is available."));
}

function equipmentMatches(item, group, query) {
  if (!query) return true;
  const points = (item.points || []).map((point) => `${point.name} ${point.category}`).join(" ");
  return `${group.name} ${item.name} ${item.variant} ${item.protocol} ${item.networkName} ${item.objectReference} ${points}`.toLocaleLowerCase().includes(query);
}

function allEquipment() {
  return (state.inventory?.groups || []).flatMap((group) => group.equipment || []);
}

function treeSubtitle(item) {
  const parts = [variantLabel(item.variant)];
  if (item.macAddress != null) parts.push(`MAC ${item.macAddress}`);
  return parts.join(" · ");
}

function variantLabel(variant) {
  const labels = {
    fanPoweredHeating: "Fan-powered heating",
    heating: "Heating",
    coolingOnly: "Cooling only",
    lightingPanel: "Lighting panel"
  };
  return labels[variant] || String(variant || "Equipment").replace(/([a-z])([A-Z])/g, "$1 $2");
}

function variantClass(variant) {
  if (variant === "fanPoweredHeating") return "fan";
  if (variant === "heating") return "heat";
  if (variant === "coolingOnly") return "cool";
  return "lighting";
}

function statusClass(value) {
  const normalized = String(value || "").toLocaleLowerCase();
  if (normalized.includes("active")) return "active";
  if (normalized.includes("offline") || normalized.includes("unavailable")) return "warning";
  return "";
}

function categoryLabel(category) {
  const labels = {
    temperature: "Temperature",
    airflow: "Airflow",
    pressure: "Pressure",
    command: "Command",
    status: "Status",
    setpoint: "Setpoint",
    mode: "Mode",
    configuration: "Configuration",
    lightingStatus: "Channel status",
    lightingCommand: "Run command",
    digitalInput: "Digital input"
  };
  return labels[category] || category || "Point";
}

function categoryClass(category) {
  if (["command", "lightingCommand"].includes(category)) return "command";
  if (["status", "lightingStatus", "digitalInput"].includes(category)) return "status";
  if (["temperature", "airflow", "pressure"].includes(category)) return "sensor";
  return "setting";
}

function tableMessage(message) {
  const row = document.createElement("tr");
  const cell = element("td", "empty-state", message);
  cell.colSpan = 6;
  row.append(cell);
  return row;
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function formatDate(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Unknown";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(date);
}

function formatInteger(value) {
  return new Intl.NumberFormat().format(value || 0);
}

function formatPointValue(value, category) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) return "Unavailable";
  let maximumFractionDigits = 2;
  if (["temperature", "setpoint"].includes(category)) maximumFractionDigits = 1;
  if (["airflow", "status", "command", "mode", "lightingStatus", "lightingCommand", "digitalInput"].includes(category)) maximumFractionDigits = 0;
  return new Intl.NumberFormat(undefined, { maximumFractionDigits }).format(numeric);
}

function formatTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Unknown";
  return new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" }).format(date);
}

function formatRelativeTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Unknown";
  const seconds = Math.round((date.getTime() - Date.now()) / 1000);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (Math.abs(seconds) < 60) return formatter.format(seconds, "second");
  const minutes = Math.round(seconds / 60);
  if (Math.abs(minutes) < 60) return formatter.format(minutes, "minute");
  const hours = Math.round(minutes / 60);
  if (Math.abs(hours) < 24) return formatter.format(hours, "hour");
  return formatter.format(Math.round(hours / 24), "day");
}

function showMessage(message, error = false) {
  const node = $("page-message");
  node.hidden = !message;
  node.textContent = message || "";
  node.classList.toggle("error", error);
}

async function fetchJson(url) {
  const response = await fetch(url, { credentials: "same-origin", cache: "no-store", headers: { Accept: "application/json" } });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || body.message || `${response.status} ${response.statusText}`);
  return body;
}
