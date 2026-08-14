"use strict";

const integerFormat = new Intl.NumberFormat();
const decimalFormat = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
const MAX_ALARM_ROWS = 1_000;
const MAX_EQUIPMENT_ROWS = 750;
const state = {
  session: null,
  data: null,
  activeTab: "triage",
  filteredAlarms: [],
  forcedAlarmFilter: "",
  selectedEquipment: "",
  refreshTimer: null
};

const $ = (id) => document.getElementById(id);

document.addEventListener("DOMContentLoaded", async () => {
  bindEvents();
  try {
    const session = await fetchJson("/api/portal/me");
    if (!["admin", "operator"].includes(session.user.role)) {
      window.location.assign("/");
      return;
    }
    state.session = session;
    window.MetasysNavigation?.configure(session);
    window.MetasysNavigation?.setActive("diagnostics");
    selectTab(tabFromHash(), false);
    await loadDiagnostics();
    state.refreshTimer = window.setInterval(loadDiagnostics, 60_000);
  } catch (error) {
    showError(error.message || "Unable to open diagnostics");
  }
});

function bindEvents() {
  document.querySelectorAll("[data-tab-target]").forEach((button) => {
    button.addEventListener("click", () => selectTab(button.dataset.tabTarget));
  });
  window.addEventListener("hashchange", () => selectTab(tabFromHash(), false));
  window.addEventListener("visibilitychange", () => {
    if (!document.hidden && state.session) loadDiagnostics();
  });
  $("diagnostic-refresh").addEventListener("click", refreshMetasys);
  $("open-all-alarms").addEventListener("click", () => selectTab("alarms"));
  for (const id of [
    "alarm-search",
    "alarm-state-filter",
    "alarm-severity-filter",
    "alarm-ack-filter",
    "alarm-type-filter",
    "alarm-system-filter"
  ]) {
    $(id).addEventListener(id === "alarm-search" ? "input" : "change", () => {
      state.forcedAlarmFilter = "";
      renderAlarmExplorer();
    });
  }
  $("clear-alarm-filters").addEventListener("click", clearAlarmFilters);
  $("export-alarms").addEventListener("click", exportFilteredAlarms);
  $("equipment-search").addEventListener("input", renderEquipment);
  $("exception-kind-filter").addEventListener("change", renderExceptions);
  $("alarm-detail-close").addEventListener("click", () => $("alarm-detail-dialog").close());
  if ("ResizeObserver" in window) {
    let resizeTimer;
    new ResizeObserver(() => {
      window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(drawVisibleCharts, 80);
    }).observe(document.querySelector(".diagnostic-main"));
  }
}

async function loadDiagnostics() {
  try {
    const data = await fetchJson("/api/diagnostics");
    state.data = data;
    hideError();
    renderAll();
  } catch (error) {
    showError(error.message || "Diagnostics API unavailable");
  }
}

async function refreshMetasys() {
  const button = $("diagnostic-refresh");
  button.disabled = true;
  button.textContent = "Refresh queued";
  try {
    const headers = new Headers();
    if (state.session?.csrfToken) headers.set("X-CSRF-Token", state.session.csrfToken);
    const response = await window.fetch("/api/refresh", {
      method: "POST",
      headers,
      credentials: "same-origin"
    });
    if (!response.ok) throw new Error(await responseError(response));
    window.setTimeout(loadDiagnostics, 2_000);
    window.setTimeout(loadDiagnostics, 15_000);
  } catch (error) {
    showError(error.message || "Unable to schedule a Metasys refresh");
  } finally {
    window.setTimeout(() => {
      button.disabled = false;
      button.textContent = "Refresh data";
    }, 1_000);
  }
}

function renderAll() {
  const data = state.data;
  renderHeader(data);
  renderSummary(data.summary, data.pollHealth, data.exceptionFeed);
  renderFindings(data.findings || []);
  populateAlarmFilters(data);
  renderTriageAlarms(data.alarms || []);
  renderAlarmExplorer();
  renderEquipment();
  renderSystems(data.systems || []);
  renderBreakdown("alarm-type-breakdown", data.alarmTypes || []);
  renderBreakdown("category-breakdown", data.categories || []);
  renderBreakdown("source-breakdown", data.sources || []);
  renderExceptions();
  renderReliability(data);
  drawVisibleCharts();
}

function renderHeader(data) {
  const health = data.health || {};
  const badge = $("diagnostic-health");
  badge.className = `health-pill ${health.state || "starting"}`;
  badge.replaceChildren(element("i"), document.createTextNode(` ${healthLabel(health.state)}`));
  $("diagnostic-connector").textContent = [health.connector, health.serverVersion].filter(Boolean).join(" · ") || "Metasys source";
  $("diagnostic-updated").textContent = health.lastSuccessAt ? `Data ${relativeTime(health.lastSuccessAt)}` : "No successful poll yet";
  const quality = data.dataQuality || {};
  $("diagnostic-history-scope").textContent = quality.historyStartedAt
    ? `${formatDateTime(quality.historyStartedAt)} → ${formatDateTime(quality.historyEndedAt)}`
    : "No alarm history cached";
}

function renderSummary(summary, pollHealth, exceptionFeed) {
  setText("summary-active", formatInteger(summary.activeAlarmCount));
  setText("summary-active-note", `${formatInteger(summary.criticalActiveCount)} critical · ${formatInteger(summary.staleActiveCount)} over 24h`);
  setText("summary-high-unack", formatInteger(summary.highPriorityUnacknowledgedActiveCount));
  setText("summary-fault", formatInteger(summary.faultActiveCount));
  setText("summary-offline", formatInteger(summary.offlineActiveCount));
  setText("summary-exceptions", exceptionFeed.state === "available" ? formatInteger(summary.pointExceptionCount) : "Unknown");
  setText("summary-exception-note", exceptionFeed.state === "available" ? `${formatInteger(summary.overrideCount)} operator overrides` : "Feed unavailable");
  setText("summary-poll-success", pollHealth.attempts ? `${decimalFormat.format(pollHealth.successPercentage)}%` : "—");
}

function renderFindings(findings) {
  const fragment = document.createDocumentFragment();
  for (const finding of findings) {
    const button = element("button", `finding-item ${finding.severity}`);
    button.type = "button";
    button.append(element("span", "finding-marker", finding.severity === "critical" ? "!!" : finding.severity === "high" ? "!" : finding.severity === "medium" ? "△" : "i"));
    const copy = element("span", "finding-copy");
    copy.append(
      element("strong", "", finding.title),
      element("span", "", finding.detail),
      element("small", "", finding.recommendation)
    );
    button.append(copy, element("span", "finding-count", formatInteger(finding.count)));
    button.addEventListener("click", () => openFinding(finding));
    fragment.append(button);
  }
  if (!findings.length) fragment.append(element("p", "diagnostic-empty", "No automated diagnostic findings are active."));
  $("findings-list").replaceChildren(fragment);
  $("findings-count").textContent = `${formatInteger(findings.length)} finding${findings.length === 1 ? "" : "s"}`;
}

function openFinding(finding) {
  if (finding.tab === "alarms") {
    clearAlarmFilters(false);
    state.forcedAlarmFilter = finding.filter;
  }
  selectTab(finding.tab || "triage");
  if (finding.tab === "alarms") renderAlarmExplorer();
}

function renderTriageAlarms(alarms) {
  const visible = alarms.filter((alarm) => alarm.active).slice(0, 20);
  const fragment = document.createDocumentFragment();
  for (const alarm of visible) {
    const row = element("tr");
    row.dataset.selectable = "true";
    row.append(
      tableCell(severityPill(alarm)),
      tableCell(stack(alarm.equipment, alarm.point)),
      tableCell(stack(alarm.message, alarm.alarmType)),
      tableCell(element("span", "", formatDateTime(alarm.occurredAt)))
    );
    row.addEventListener("click", () => showAlarmDetail(alarm));
    fragment.append(row);
  }
  if (!visible.length) fragment.append(emptyRow(4, "No active alarms were returned by the latest poll."));
  $("triage-alarm-body").replaceChildren(fragment);
}

function populateAlarmFilters(data) {
  preserveSelectOptions($("alarm-type-filter"), "All types", uniqueSorted((data.alarmTypes || []).map((item) => item.label)));
  preserveSelectOptions($("alarm-system-filter"), "All systems", uniqueSorted((data.systems || []).map((item) => item.system)));
}

function renderAlarmExplorer() {
  if (!state.data) return;
  const query = $("alarm-search").value.trim().toLocaleLowerCase();
  const stateFilter = $("alarm-state-filter").value;
  const severityFilter = $("alarm-severity-filter").value;
  const ackFilter = $("alarm-ack-filter").value;
  const typeFilter = $("alarm-type-filter").value;
  const systemFilter = $("alarm-system-filter").value;
  const filtered = (state.data.alarms || []).filter((alarm) => {
    const searchable = `${alarm.equipment} ${alarm.point} ${alarm.message} ${alarm.alarmType} ${alarm.category} ${alarm.objectId} ${alarm.system} ${alarm.source}`.toLocaleLowerCase();
    if (query && !searchable.includes(query)) return false;
    if (stateFilter === "active" && !alarm.active) return false;
    if (stateFilter === "returned" && alarm.active) return false;
    if (stateFilter === "stale" && !alarm.stale) return false;
    if (severityFilter !== "all" && alarm.severity !== severityFilter) return false;
    if (ackFilter === "acknowledged" && !alarm.acknowledged) return false;
    if (ackFilter === "unacknowledged" && alarm.acknowledged) return false;
    if (typeFilter !== "all" && alarm.alarmType !== typeFilter) return false;
    if (systemFilter !== "all" && alarm.system !== systemFilter) return false;
    return matchesForcedAlarmFilter(alarm, state.forcedAlarmFilter);
  });
  state.filteredAlarms = filtered;
  const visible = filtered.slice(0, MAX_ALARM_ROWS);
  const fragment = document.createDocumentFragment();
  for (const alarm of visible) {
    const row = element("tr");
    row.dataset.selectable = "true";
    row.append(
      tableCell(statePill(alarm)),
      tableCell(stack(alarm.equipment, `${alarm.point} · ${originLabel(alarm.equipmentOrigin)}`)),
      tableCell(stack(alarm.message, alarm.objectId)),
      tableCell(stack(alarm.alarmType, alarm.category)),
      tableCell(severityPill(alarm)),
      tableCell(stack(formatDateTime(alarm.occurredAt), relativeTime(alarm.occurredAt))),
      tableCell(element("span", `ack-icon${alarm.acknowledged ? "" : " no"}`, alarm.acknowledged ? "Yes" : "No")),
      tableCell(element("span", "numeric-cell", formatInteger(alarm.occurrenceCount)))
    );
    row.addEventListener("click", () => showAlarmDetail(alarm));
    fragment.append(row);
  }
  if (!visible.length) fragment.append(emptyRow(8, "No alarm records match these filters."));
  $("alarm-explorer-body").replaceChildren(fragment);
  const suffix = filtered.length > MAX_ALARM_ROWS ? ` · first ${formatInteger(MAX_ALARM_ROWS)} shown` : "";
  const finding = state.forcedAlarmFilter ? ` · diagnostic filter: ${state.forcedAlarmFilter}` : "";
  $("alarm-result-count").textContent = `${formatInteger(filtered.length)} record${filtered.length === 1 ? "" : "s"}${suffix}${finding}`;
}

function matchesForcedAlarmFilter(alarm, filter) {
  if (!filter) return true;
  if (filter === "critical") return alarm.active && alarm.priority <= 39;
  if (filter === "highUnacknowledged") return alarm.active && !alarm.acknowledged && alarm.priority <= 79;
  if (filter === "offline") return alarm.active && ["offline", "communication"].some((value) => alarm.alarmType.toLocaleLowerCase().includes(value));
  if (filter === "fault") return alarm.active && ["fault", "unreliable"].some((value) => alarm.alarmType.toLocaleLowerCase().includes(value));
  if (filter === "stale") return alarm.stale;
  return true;
}

function clearAlarmFilters(render = true) {
  $("alarm-search").value = "";
  $("alarm-state-filter").value = "all";
  $("alarm-severity-filter").value = "all";
  $("alarm-ack-filter").value = "all";
  $("alarm-type-filter").value = "all";
  $("alarm-system-filter").value = "all";
  state.forcedAlarmFilter = "";
  if (render) renderAlarmExplorer();
}

function showAlarmDetail(alarm) {
  $("alarm-detail-title").textContent = `${alarm.equipment} · ${alarm.point}`;
  const fields = [
    ["Current state", alarm.active ? "Active" : "Returned / history"],
    ["Severity / priority", `${alarm.severity} / ${alarm.priority}`],
    ["Acknowledged", alarm.acknowledged ? "Yes" : "No"],
    ["Occurrence count", String(alarm.occurrenceCount)],
    ["Alarm type", alarm.alarmType],
    ["Category", alarm.category],
    ["Detected", formatDateTime(alarm.occurredAt)],
    ["Last collected", alarm.lastSeenAt ? formatDateTime(alarm.lastSeenAt) : "Not recorded"],
    ["Equipment mapping", originLabel(alarm.equipmentOrigin)],
    ["Connector source", alarm.source],
    ["System / controller path", alarm.system, true],
    ["Message", alarm.message, true],
    ["Object reference", alarm.objectId || "Unavailable", true],
    ["Event identifier", alarm.id, true]
  ];
  const fragment = document.createDocumentFragment();
  for (const [label, value, wide] of fields) {
    const field = element("div", `alarm-detail-field${wide ? " wide" : ""}`);
    field.append(element("span", "", label), element("strong", "", value));
    fragment.append(field);
  }
  $("alarm-detail-content").replaceChildren(fragment);
  $("alarm-detail-dialog").showModal();
}

function renderEquipment() {
  if (!state.data) return;
  const query = $("equipment-search").value.trim().toLocaleLowerCase();
  const matches = (state.data.equipment || []).filter((item) => `${item.equipment} ${item.system} ${item.topCondition}`.toLocaleLowerCase().includes(query));
  const visible = matches.slice(0, MAX_EQUIPMENT_ROWS);
  if (visible.length && !visible.some((item) => item.equipment === state.selectedEquipment)) {
    selectEquipment(visible[0], false);
  }
  const fragment = document.createDocumentFragment();
  for (const item of visible) {
    const row = element("tr", state.selectedEquipment === item.equipment ? "selected" : "");
    row.dataset.selectable = "true";
    row.append(
      tableCell(stack(item.equipment, `${originLabel(item.equipmentOrigin)} · ${item.topCondition}`)),
      tableCell(element("span", "object-reference", item.system)),
      tableCell(numberNode(item.pointCount)),
      tableCell(numberNode(item.activeCount)),
      tableCell(numberNode(item.highPriorityCount)),
      tableCell(numberNode(item.faultCount)),
      tableCell(numberNode(item.offlineCount)),
      tableCell(numberNode(item.historyCount)),
      tableCell(element("strong", "numeric-cell", decimalFormat.format(item.score)))
    );
    row.addEventListener("click", () => selectEquipment(item));
    fragment.append(row);
  }
  if (!visible.length) fragment.append(emptyRow(9, "No equipment groups match this search."));
  $("equipment-body").replaceChildren(fragment);
}

function selectEquipment(item, rerender = true) {
  state.selectedEquipment = item.equipment;
  $("equipment-detail-name").textContent = item.equipment;
  const score = element("div", "detail-score");
  score.append(element("span", "", `${item.system}\nDiagnostic score`), element("strong", "", decimalFormat.format(item.score)));
  const grid = element("div", "detail-grid");
  for (const [label, value] of [
    ["Points", item.pointCount], ["History", item.historyCount], ["Active", item.activeCount],
    ["Unacknowledged", item.unacknowledgedCount], ["High priority", item.highPriorityCount],
    ["Fault", item.faultCount], ["Offline", item.offlineCount], ["Last alarm", relativeTime(item.lastAlarmAt)]
  ]) {
    const cell = element("div");
    cell.append(element("span", "", label), element("strong", "", typeof value === "number" ? formatInteger(value) : value));
    grid.append(cell);
  }
  const action = element("button", "diagnostic-button secondary detail-action", "View related alarms");
  action.type = "button";
  action.addEventListener("click", () => {
    clearAlarmFilters(false);
    $("alarm-search").value = item.equipment;
    selectTab("alarms");
    renderAlarmExplorer();
  });
  $("equipment-detail").replaceChildren(score, element("p", "diagnostic-empty", `Dominant condition: ${item.topCondition} · Mapping: ${originLabel(item.equipmentOrigin)}`), grid, action);
  if (rerender) renderEquipment();
}

function renderSystems(systems) {
  const fragment = document.createDocumentFragment();
  for (const system of systems) {
    const item = element("article", "system-item");
    const stats = element("p");
    stats.append(
      stat(`${formatInteger(system.equipmentCount)} equipment`),
      stat(`${formatInteger(system.pointCount)} points`),
      stat(`${formatInteger(system.activeCount)} active`),
      stat(`${formatInteger(system.highPriorityCount)} high`),
      stat(`${formatInteger(system.historyCount)} history`)
    );
    item.append(element("strong", "", system.system), stats);
    fragment.append(item);
  }
  if (!systems.length) fragment.append(element("p", "diagnostic-empty", "No system paths are available."));
  $("systems-grid").replaceChildren(fragment);
  $("system-count").textContent = `${formatInteger(systems.length)} system${systems.length === 1 ? "" : "s"}`;
}

function renderBreakdown(id, items) {
  const fragment = document.createDocumentFragment();
  for (const item of items) {
    const row = element("div", "breakdown-item");
    const copy = element("div", "breakdown-copy");
    const bar = element("div", "breakdown-bar");
    const fill = element("i");
    fill.style.width = `${Math.max(1, item.percentage)}%`;
    bar.append(fill);
    copy.append(element("span", "", item.label), bar);
    const value = element("div", "breakdown-value", formatInteger(item.count));
    value.append(element("small", "", `${decimalFormat.format(item.percentage)}%`));
    row.append(copy, value);
    fragment.append(row);
  }
  if (!items.length) fragment.append(element("p", "diagnostic-empty", "No records are available."));
  $(id).replaceChildren(fragment);
}

function renderExceptions() {
  if (!state.data) return;
  const feed = state.data.exceptionFeed;
  const banner = $("exception-feed-banner");
  banner.className = `feed-banner ${feed.state}`;
  $("exception-feed-title").textContent = feed.state === "available" ? "Current point-exception feed available" : "Current point-exception feed unavailable";
  $("exception-feed-message").textContent = feed.message;
  renderOverrides(state.data.overrides || [], feed);

  const kind = $("exception-kind-filter").value;
  const records = (state.data.pointExceptions || []).filter((record) => kind === "all" || record.kind === kind);
  const fragment = document.createDocumentFragment();
  for (const record of records) {
    const row = element("tr");
    row.append(
      tableCell(element("span", `kind-pill ${record.kind}`, kindLabel(record.kind))),
      tableCell(stack(record.equipment, record.point)),
      tableCell(element("span", "", record.value || "—")),
      tableCell(element("span", "", record.status || "Not normal")),
      tableCell(numberNode(record.statusId ?? "—")),
      tableCell(element("span", "", record.expiresAt ? formatDateTime(record.expiresAt) : "No expiry")),
      tableCell(element("span", "object-reference", record.objectId || "Unavailable"))
    );
    fragment.append(row);
  }
  if (!records.length) {
    const message = feed.state === "available" ? "No current point exceptions match this filter." : "Exception records cannot be verified until the feed recovers.";
    fragment.append(emptyRow(7, message));
  }
  $("exception-body").replaceChildren(fragment);
}

function renderOverrides(overrides, feed) {
  const fragment = document.createDocumentFragment();
  for (const record of overrides) {
    const row = element("tr");
    row.append(
      tableCell(stack(record.equipment, record.point)),
      tableCell(element("span", "", record.value || "—")),
      tableCell(element("span", "kind-pill override", record.status || "Operator override")),
      tableCell(element("span", "", record.startedAt ? formatDateTime(record.startedAt) : "Not supplied")),
      tableCell(element("span", "", record.expiresAt ? formatDateTime(record.expiresAt) : "No expiry")),
      tableCell(element("span", "object-reference", record.objectId || "Unavailable"))
    );
    fragment.append(row);
  }
  if (!overrides.length) {
    const message = feed.state === "available" ? "The current scan returned no active operator overrides." : "Override count is unknown because the current exception scan failed.";
    fragment.append(emptyRow(6, message));
  }
  $("override-body").replaceChildren(fragment);
  $("override-total").textContent = feed.state === "available" ? `${formatInteger(overrides.length)} active` : "Unknown";
}

function renderReliability(data) {
  const poll = data.pollHealth;
  const quality = data.dataQuality;
  setText("reliability-success", poll.attempts ? `${decimalFormat.format(poll.successPercentage)}%` : "—");
  setText("reliability-attempts", `${formatInteger(poll.successes)} successful · ${formatInteger(poll.failures)} failed`);
  setText("reliability-average", formatDuration(poll.averageDurationMs));
  setText("reliability-maximum", formatDuration(poll.maximumDurationMs));
  setText("mapping-percentage", `${decimalFormat.format(quality.equipmentMappingPercentage)}%`);
  setText("mapping-detail", `${formatInteger(quality.serverMappedEquipment)} server · ${formatInteger(quality.inferredEquipment)} inferred`);
  setText("quality-points", formatInteger(quality.distinctPoints));
  setText("quality-systems", `${formatInteger(quality.distinctEquipment)} equipment · ${formatInteger(quality.distinctSystems)} systems`);
  renderPollFailures(poll.failuresDetail || []);
  renderCapabilities(quality.capabilities || []);
  renderQuality(quality);
}

function renderPollFailures(failures) {
  const fragment = document.createDocumentFragment();
  for (const failure of failures) {
    const item = element("article", "failure-item");
    item.append(element("time", "", formatDateTime(failure.attemptedAt)), element("p", "", failure.message));
    fragment.append(item);
  }
  if (!failures.length) fragment.append(element("p", "diagnostic-empty", "No poll failures are recorded in the seven-day window."));
  $("poll-failure-list").replaceChildren(fragment);
  $("failure-count").textContent = `${formatInteger(failures.length)} shown`;
}

function renderCapabilities(capabilities) {
  const fragment = document.createDocumentFragment();
  for (const capability of capabilities) {
    const item = element("article", "capability-item");
    const copy = element("div");
    copy.append(element("strong", "", capability.name), element("p", "", capability.detail));
    item.append(copy, element("span", `capability-state ${capability.state}`, capabilityStateLabel(capability.state)));
    fragment.append(item);
  }
  $("capability-list").replaceChildren(fragment);
}

function renderQuality(quality) {
  const rows = [
    ["Object references", quality.objectReferencePercentage],
    ["Alarm messages", quality.messagePercentage],
    ["Categories", quality.categoryPercentage],
    ["Server equipment mapping", quality.equipmentMappingPercentage]
  ];
  const fragment = document.createDocumentFragment();
  for (const [label, value] of rows) {
    const row = element("div", "quality-row");
    const track = element("div", "quality-track");
    const fill = element("i");
    fill.style.width = `${Math.max(0, Math.min(100, value))}%`;
    track.append(fill);
    row.append(element("span", "", label), track, element("strong", "", `${decimalFormat.format(value)}%`));
    fragment.append(row);
  }
  const coverage = element("p", "diagnostic-empty", quality.historyStartedAt ? `History coverage: ${formatDateTime(quality.historyStartedAt)} through ${formatDateTime(quality.historyEndedAt)} · ${formatInteger(quality.totalRecords)} records.` : "No history coverage is available.");
  fragment.append(coverage);
  $("quality-list").replaceChildren(fragment);
}

function drawVisibleCharts() {
  if (!state.data) return;
  if (state.activeTab === "patterns") {
    drawDailyChart($("daily-activity-chart"), state.data.dailyActivity || []);
    drawHourlyChart($("hourly-activity-chart"), state.data.hourlyActivityUtc || []);
  }
  if (state.activeTab === "reliability") drawPollChart($("poll-activity-chart"), state.data.pollHealth.activity || []);
}

function drawDailyChart(canvas, points) {
  const chart = chartContext(canvas, 44, 15, 17, 28);
  if (!chart) return;
  const { ctx, width, height, left, top, right, bottom } = chart;
  const maximum = Math.max(1, ...points.flatMap((point) => [point.total, point.highPriority, point.normalReturns]));
  drawGrid(ctx, width, height, left, top, right, bottom, maximum);
  drawLine(ctx, points.map((point) => point.total), maximum, chart, "#35c7cf", false);
  drawLine(ctx, points.map((point) => point.highPriority), maximum, chart, "#ef8d59", false);
  drawLine(ctx, points.map((point) => point.normalReturns), maximum, chart, "#60d29b", true);
  ctx.fillStyle = "#58727d";
  ctx.font = "8px ui-sans-serif";
  ctx.textAlign = "center";
  for (let index = 0; index < points.length; index += 5) {
    const x = left + (width - left - right) * index / Math.max(1, points.length - 1);
    ctx.fillText(shortDate(points[index].date), x, height - 7);
  }
}

function drawHourlyChart(canvas, points) {
  const chart = chartContext(canvas, 36, 12, 14, 26);
  if (!chart) return;
  const { ctx, width, height, left, top, right, bottom } = chart;
  const maximum = Math.max(1, ...points.map((point) => point.count));
  drawGrid(ctx, width, height, left, top, right, bottom, maximum);
  const plotWidth = width - left - right;
  const plotHeight = height - top - bottom;
  const slot = plotWidth / Math.max(1, points.length);
  points.forEach((point, index) => {
    const barHeight = plotHeight * point.count / maximum;
    ctx.fillStyle = index % 2 ? "#2c8992" : "#35c7cf";
    ctx.fillRect(left + index * slot + 1, top + plotHeight - barHeight, Math.max(2, slot - 2), barHeight);
  });
  ctx.fillStyle = "#58727d";
  ctx.font = "8px ui-sans-serif";
  ctx.textAlign = "center";
  for (let hour = 0; hour < 24; hour += 4) ctx.fillText(String(hour).padStart(2, "0"), left + (hour + .5) * slot, height - 7);
}

function drawPollChart(canvas, points) {
  const chart = chartContext(canvas, 42, 12, 14, 28);
  if (!chart) return;
  const { ctx, width, height, left, top, right, bottom } = chart;
  const maximum = Math.max(1, ...points.map((point) => point.maximumActiveAlarms));
  drawGrid(ctx, width, height, left, top, right, bottom, maximum);
  drawLine(ctx, points.map((point) => point.maximumActiveAlarms), maximum, chart, "#35c7cf", false);
  const plotWidth = width - left - right;
  const plotHeight = height - top - bottom;
  const slot = plotWidth / Math.max(1, points.length);
  points.forEach((point, index) => {
    if (!point.failures) return;
    ctx.fillStyle = "rgba(239,98,106,.8)";
    ctx.fillRect(left + index * slot + slot * .3, top + plotHeight - 9, Math.max(3, slot * .4), 9);
  });
  ctx.fillStyle = "#58727d";
  ctx.font = "8px ui-sans-serif";
  ctx.textAlign = "center";
  points.forEach((point, index) => {
    if (index % 4 !== 0) return;
    ctx.fillText(new Date(point.hour).toLocaleTimeString([], { hour: "numeric" }), left + index * slot, height - 7);
  });
}

function chartContext(canvas, left, top, right, bottom) {
  const rect = canvas.getBoundingClientRect();
  if (!rect.width || !rect.height) return null;
  const ratio = Math.min(window.devicePixelRatio || 1, 2);
  canvas.width = Math.round(rect.width * ratio);
  canvas.height = Math.round(rect.height * ratio);
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, rect.width, rect.height);
  return { ctx, width: rect.width, height: rect.height, left, top, right, bottom };
}

function drawGrid(ctx, width, height, left, top, right, bottom, maximum) {
  const plotHeight = height - top - bottom;
  ctx.strokeStyle = "rgba(126,164,178,.12)";
  ctx.fillStyle = "#58727d";
  ctx.font = "8px ui-sans-serif";
  ctx.textAlign = "right";
  for (let index = 0; index <= 4; index += 1) {
    const y = top + plotHeight * index / 4;
    ctx.beginPath();
    ctx.moveTo(left, y);
    ctx.lineTo(width - right, y);
    ctx.stroke();
    ctx.fillText(formatInteger(Math.round(maximum * (1 - index / 4))), left - 6, y + 3);
  }
}

function drawLine(ctx, values, maximum, chart, color, dashed) {
  if (!values.length) return;
  const plotWidth = chart.width - chart.left - chart.right;
  const plotHeight = chart.height - chart.top - chart.bottom;
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.7;
  ctx.setLineDash(dashed ? [4, 4] : []);
  ctx.beginPath();
  values.forEach((value, index) => {
    const x = chart.left + plotWidth * index / Math.max(1, values.length - 1);
    const y = chart.top + plotHeight - plotHeight * value / maximum;
    if (index === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  });
  ctx.stroke();
  ctx.setLineDash([]);
}

function selectTab(tab, updateHash = true) {
  const valid = ["triage", "alarms", "equipment", "patterns", "exceptions", "reliability"];
  state.activeTab = valid.includes(tab) ? tab : "triage";
  document.querySelectorAll("[data-tab-target]").forEach((button) => {
    const active = button.dataset.tabTarget === state.activeTab;
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
    button.tabIndex = active ? 0 : -1;
  });
  document.querySelectorAll("[data-tab-panel]").forEach((panel) => {
    panel.hidden = panel.dataset.tabPanel !== state.activeTab;
  });
  if (updateHash && window.location.hash !== `#${state.activeTab}`) window.history.replaceState(null, "", `#${state.activeTab}`);
  window.setTimeout(drawVisibleCharts, 0);
}

function tabFromHash() {
  return window.location.hash.slice(1) || "triage";
}

function exportFilteredAlarms() {
  const columns = ["state", "severity", "priority", "acknowledged", "equipment", "equipment_origin", "point", "message", "alarm_type", "category", "detected_at", "last_collected_at", "occurrence_count", "system", "object_reference", "source", "event_id"];
  const rows = state.filteredAlarms.map((alarm) => [
    alarm.active ? "active" : "returned", alarm.severity, alarm.priority, alarm.acknowledged,
    alarm.equipment, alarm.equipmentOrigin, alarm.point, alarm.message, alarm.alarmType, alarm.category,
    alarm.occurredAt, alarm.lastSeenAt || "", alarm.occurrenceCount, alarm.system, alarm.objectId, alarm.source, alarm.id
  ]);
  const csv = [columns, ...rows].map((row) => row.map(csvValue).join(",")).join("\r\n");
  const url = URL.createObjectURL(new Blob([csv], { type: "text/csv;charset=utf-8" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = `metasys-diagnostics-${new Date().toISOString().slice(0, 10)}.csv`;
  link.click();
  URL.revokeObjectURL(url);
}

async function fetchJson(resource) {
  const response = await window.fetch(resource, { cache: "no-store", credentials: "same-origin" });
  if (response.status === 401) {
    window.location.assign("/");
    throw new Error("Sign in required");
  }
  if (!response.ok) throw new Error(await responseError(response));
  return response.json();
}

async function responseError(response) {
  try {
    const body = await response.json();
    return body.error || `Request failed (${response.status})`;
  } catch (_) {
    return `Request failed (${response.status})`;
  }
}

function showError(message) {
  $("diagnostic-error").textContent = message;
  $("diagnostic-error").hidden = false;
}

function hideError() {
  $("diagnostic-error").hidden = true;
  $("diagnostic-error").textContent = "";
}

function preserveSelectOptions(select, allLabel, values) {
  const current = select.value;
  const fragment = document.createDocumentFragment();
  fragment.append(option("all", allLabel));
  for (const value of values) fragment.append(option(value, value));
  select.replaceChildren(fragment);
  select.value = values.includes(current) ? current : "all";
}

function severityPill(alarm) {
  const label = alarm.severity === "critical" ? `Critical · ${alarm.priority}` : `${titleCase(alarm.severity)} · ${alarm.priority}`;
  return element("span", `severity-pill ${alarm.severity}`, label);
}

function statePill(alarm) {
  return element("span", `state-pill ${alarm.active ? "active" : "returned"}`, alarm.active ? (alarm.stale ? "Active 24h+" : "Active") : "Returned");
}

function stack(primary, secondary) {
  const node = element("span", "cell-stack");
  node.append(element("strong", "", primary || "Unknown"), element("small", "", secondary || ""));
  return node;
}

function tableCell(content) {
  const cell = element("td");
  if (content instanceof Node) cell.append(content);
  else cell.textContent = String(content ?? "");
  return cell;
}

function emptyRow(columns, message) {
  const row = element("tr");
  const cell = element("td", "diagnostic-empty-cell", message);
  cell.colSpan = columns;
  row.append(cell);
  return row;
}

function numberNode(value) {
  return element("span", "numeric-cell", typeof value === "number" ? formatInteger(value) : String(value));
}

function stat(text) {
  const node = element("span");
  const [value, ...label] = text.split(" ");
  node.append(element("b", "", value), document.createTextNode(` ${label.join(" ")}`));
  return node;
}

function option(value, label) {
  const node = document.createElement("option");
  node.value = value;
  node.textContent = label;
  return node;
}

function element(tag, className = "", text = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== "") node.textContent = text;
  return node;
}

function uniqueSorted(values) {
  return [...new Set(values.filter(Boolean))].sort((left, right) => left.localeCompare(right));
}

function formatInteger(value) {
  return integerFormat.format(Number(value || 0));
}

function formatDuration(milliseconds) {
  if (milliseconds === null || milliseconds === undefined) return "Collecting";
  if (milliseconds < 1_000) return `${formatInteger(milliseconds)} ms`;
  return `${decimalFormat.format(milliseconds / 1_000)} s`;
}

function formatDateTime(value) {
  if (!value) return "Unavailable";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value);
  return date.toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
}

function relativeTime(value) {
  const difference = Date.now() - new Date(value).getTime();
  if (!Number.isFinite(difference)) return "unknown time";
  const future = difference < 0;
  const absolute = Math.abs(difference);
  let amount;
  let unit;
  if (absolute < 60_000) { amount = Math.max(1, Math.round(absolute / 1_000)); unit = "sec"; }
  else if (absolute < 3_600_000) { amount = Math.round(absolute / 60_000); unit = "min"; }
  else if (absolute < 86_400_000) { amount = Math.round(absolute / 3_600_000); unit = "hr"; }
  else { amount = Math.round(absolute / 86_400_000); unit = "day"; }
  return future ? `in ${amount} ${unit}` : `${amount} ${unit} ago`;
}

function shortDate(value) {
  const date = new Date(`${value}T00:00:00Z`);
  return date.toLocaleDateString([], { month: "short", day: "numeric", timeZone: "UTC" });
}

function csvValue(value) {
  const original = String(value ?? "");
  const text = /^[=+\-@\t\r]/.test(original) ? `'${original}` : original;
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

function originLabel(value) {
  if (value === "server") return "Server mapped";
  if (value === "inferred") return "Inferred from reference";
  return "Mapping unknown";
}

function kindLabel(value) {
  if (value === "notNormal") return "Not normal";
  return titleCase(value);
}

function capabilityStateLabel(value) {
  if (value === "onDemand") return "On demand";
  return titleCase(value);
}

function healthLabel(value) {
  if (value === "ok") return "Current";
  if (value === "demo") return "Demo";
  if (value === "error") return "Degraded";
  return "Connecting";
}

function titleCase(value) {
  const text = String(value || "Unknown");
  return text.charAt(0).toLocaleUpperCase() + text.slice(1);
}

function setText(id, value) {
  $(id).textContent = value;
}
