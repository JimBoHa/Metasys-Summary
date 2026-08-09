"use strict";

const numberFormat = new Intl.NumberFormat();
const palette = ["#2cc7d2", "#53d18b", "#f2b84b", "#f1894c", "#9a8cff", "#ef6f98", "#65a7e8", "#627984"];
let dashboardData = null;
let refreshTimer = null;
let reportRecipients = [];

const $ = (id) => document.getElementById(id);

document.addEventListener("DOMContentLoaded", () => {
  $("refresh-button").addEventListener("click", manualRefresh);
  $("report-settings-button").addEventListener("click", openReportSettings);
  $("report-settings-close").addEventListener("click", () => $("report-settings-dialog").close());
  $("report-settings-form").addEventListener("submit", saveReportSettings);
  $("add-report-recipient").addEventListener("click", addReportRecipient);
  $("new-report-recipient").addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      addReportRecipient();
    }
  });
  $("smtp-test-button").addEventListener("click", testSmtpConnection);
  $("send-report-button").addEventListener("click", sendReportNow);
  loadDashboard();
  refreshTimer = window.setInterval(loadDashboard, 60_000);
  window.addEventListener("visibilitychange", () => {
    if (!document.hidden) loadDashboard();
  });
  if ("ResizeObserver" in window) {
    let resizeTimeout;
    new ResizeObserver(() => {
      window.clearTimeout(resizeTimeout);
      resizeTimeout = window.setTimeout(drawCharts, 80);
    }).observe(document.querySelector(".dashboard-grid"));
  }
});

async function loadDashboard() {
  try {
    const response = await fetch("/api/dashboard", { cache: "no-store" });
    if (!response.ok) throw new Error(`Dashboard API returned ${response.status}`);
    dashboardData = await response.json();
    renderDashboard(dashboardData);
  } catch (error) {
    showClientError(error.message || "Dashboard API unavailable");
  }
}

async function manualRefresh() {
  const button = $("refresh-button");
  button.disabled = true;
  button.classList.add("loading");
  try {
    await fetch("/api/refresh", { method: "POST" });
    await wait(700);
    await loadDashboard();
  } finally {
    button.disabled = false;
    button.classList.remove("loading");
  }
}

async function openReportSettings() {
  const dialog = $("report-settings-dialog");
  showReportMessage("Loading settings…");
  if (!dialog.open) dialog.showModal();
  try {
    const response = await fetch("/api/settings/reports", { cache: "no-store" });
    if (!response.ok) throw new Error(await apiError(response));
    const settings = await response.json();
    $("report-enabled").checked = settings.enabled;
    $("smtp-host").value = settings.smtpHost;
    $("smtp-port").value = settings.smtpPort;
    $("smtp-username").value = settings.smtpUsername;
    $("smtp-tls-mode").value = settings.tlsMode;
    $("report-from-name").value = settings.fromName;
    $("report-from-address").value = settings.fromAddress;
    $("smtp-password").value = "";
    $("smtp-clear-password").checked = false;
    $("report-cadence").value = settings.cadence;
    $("report-send-time").value = settings.sendTime;
    $("report-weekly-day").value = String(settings.weeklyDay);
    $("section-active-alarms").checked = settings.sections.activeAlarms;
    $("section-common-alarms").checked = settings.sections.commonAlarms;
    $("section-serious-alarms").checked = settings.sections.seriousAlarms;
    $("section-overrides").checked = settings.sections.operatorOverrides;
    $("section-problematic-equipment").checked = settings.sections.problematicEquipment;
    $("section-equipment-offline").checked = settings.sections.equipmentOffline;
    $("section-alarm-rate").checked = settings.sections.alarmRate;
    reportRecipients = [...settings.recipients];
    renderReportRecipients();
    setText("smtp-password-state", settings.passwordConfigured ? "Password saved in macOS Keychain" : "No password saved");
    renderDeliveryStatus(settings);
    showReportMessage("");
  } catch (error) {
    showReportMessage(error.message || "Unable to load report settings", true);
  }
}

function addReportRecipient() {
  const input = $("new-report-recipient");
  const value = input.value.trim().toLowerCase();
  if (!value || !input.checkValidity()) {
    input.reportValidity();
    return;
  }
  if (!reportRecipients.includes(value)) reportRecipients.push(value);
  input.value = "";
  renderReportRecipients();
}

function renderReportRecipients() {
  const list = $("report-recipient-list");
  list.replaceChildren();
  if (!reportRecipients.length) {
    const empty = document.createElement("p");
    empty.className = "empty-recipient";
    empty.textContent = "No recipients configured";
    list.append(empty);
    return;
  }
  reportRecipients.forEach((address) => {
    const chip = document.createElement("span");
    chip.className = "recipient-chip";
    const label = document.createElement("span");
    label.textContent = address;
    const remove = document.createElement("button");
    remove.type = "button";
    remove.setAttribute("aria-label", `Remove ${address}`);
    remove.textContent = "×";
    remove.addEventListener("click", () => {
      reportRecipients = reportRecipients.filter((recipient) => recipient !== address);
      renderReportRecipients();
    });
    chip.append(label, remove);
    list.append(chip);
  });
}

function reportSettingsPayload() {
  const password = $("smtp-password").value;
  return {
    enabled: $("report-enabled").checked,
    smtpHost: $("smtp-host").value,
    smtpPort: Number($("smtp-port").value),
    smtpUsername: $("smtp-username").value,
    smtpPassword: password || null,
    clearPassword: $("smtp-clear-password").checked,
    fromName: $("report-from-name").value,
    fromAddress: $("report-from-address").value,
    tlsMode: $("smtp-tls-mode").value,
    recipients: reportRecipients,
    cadence: $("report-cadence").value,
    sendTime: $("report-send-time").value,
    weeklyDay: Number($("report-weekly-day").value),
    sections: {
      activeAlarms: $("section-active-alarms").checked,
      commonAlarms: $("section-common-alarms").checked,
      seriousAlarms: $("section-serious-alarms").checked,
      operatorOverrides: $("section-overrides").checked,
      problematicEquipment: $("section-problematic-equipment").checked,
      equipmentOffline: $("section-equipment-offline").checked,
      alarmRate: $("section-alarm-rate").checked
    }
  };
}

async function saveReportSettings(event) {
  event.preventDefault();
  const form = $("report-settings-form");
  if (!form.reportValidity()) return;
  const button = $("report-save-button");
  button.disabled = true;
  showReportMessage("Saving…");
  try {
    const response = await fetch("/api/settings/reports", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(reportSettingsPayload())
    });
    if (!response.ok) throw new Error(await apiError(response));
    const settings = await response.json();
    $("smtp-password").value = "";
    $("smtp-clear-password").checked = false;
    setText("smtp-password-state", settings.passwordConfigured ? "Password saved in macOS Keychain" : "No password saved");
    renderDeliveryStatus(settings);
    showReportMessage("Report settings saved.");
  } catch (error) {
    showReportMessage(error.message || "Unable to save report settings", true);
  } finally {
    button.disabled = false;
  }
}

async function testSmtpConnection() {
  const button = $("smtp-test-button");
  button.disabled = true;
  showReportMessage("Testing saved SMTP connection…");
  try {
    const response = await fetch("/api/settings/reports/test", { method: "POST" });
    if (!response.ok) throw new Error(await apiError(response));
    showReportMessage("SMTP connection successful.");
  } catch (error) {
    showReportMessage(error.message || "SMTP connection failed", true);
  } finally {
    button.disabled = false;
  }
}

async function sendReportNow() {
  const button = $("send-report-button");
  button.disabled = true;
  showReportMessage("Building and sending report…");
  try {
    const response = await fetch("/api/reports/send", { method: "POST" });
    if (!response.ok) throw new Error(await apiError(response));
    const result = await response.json();
    showReportMessage(`Report sent to ${numberFormat.format(result.recipientCount)} recipient${result.recipientCount === 1 ? "" : "s"}.`);
    setText("report-delivery-status", `Last sent ${formatTime(result.sentAt)}`);
    $("report-delivery-status").classList.remove("error");
  } catch (error) {
    showReportMessage(error.message || "Unable to send report", true);
  } finally {
    button.disabled = false;
  }
}

function renderDeliveryStatus(settings) {
  const element = $("report-delivery-status");
  if (settings.lastError) {
    element.textContent = `Last attempt ${formatTime(settings.lastAttemptAt)} failed: ${settings.lastError}`;
    element.classList.add("error");
  } else if (settings.lastSuccessAt) {
    element.textContent = `Last successful delivery ${formatTime(settings.lastSuccessAt)}`;
    element.classList.remove("error");
  } else {
    element.textContent = "No report delivery attempted";
    element.classList.remove("error");
  }
}

function showReportMessage(message, isError = false) {
  const element = $("report-settings-message");
  element.textContent = message;
  element.classList.toggle("error", isError);
}

async function apiError(response) {
  try {
    const body = await response.json();
    return body.error || `Request failed (${response.status})`;
  } catch (_) {
    return `Request failed (${response.status})`;
  }
}

function renderDashboard(data) {
  renderHealth(data.health);
  setText("metric-active", numberFormat.format(data.activeAlarmCount));
  setText("metric-critical", numberFormat.format(data.criticalActiveCount));
  setText("metric-overrides", numberFormat.format(data.overrideCount));
  setText("metric-history", numberFormat.format(data.thirtyDayAlarmCount));
  setText("active-count-label", `${numberFormat.format(data.activeAlarmCount)} open`);
  setText("override-count-label", `${numberFormat.format(data.overrideCount)} active`);

  const topEquipment = data.problematicEquipment[0];
  setText("metric-equipment", topEquipment ? topEquipment.equipment : "No alarm data");
  setText("metric-equipment-detail", topEquipment ? `${numberFormat.format(topEquipment.alarmCount)} alarms · score ${topEquipment.score}` : "Waiting for alarm history");
  setText("history-coverage", historyCoverage(data.health.historyStartedAt));

  renderActiveAlarms(data.activeAlarms);
  renderOverrides(data.overrides);
  renderFrequent(data.frequentAlarms);
  renderSerious(data.seriousAlarms);
  renderEquipment(data.problematicEquipment);
  drawCharts();

  setText("footer-timestamp", `Snapshot ${formatTime(data.generatedAt)}`);
}

function renderHealth(health) {
  const badge = $("health-badge");
  badge.className = `status-badge ${health.state}`;
  badge.lastChild.textContent = health.state === "ok" ? " Live" : health.state === "demo" ? " Demo" : health.state === "error" ? " Attention" : " Connecting";
  setText("last-updated", health.lastSuccessAt ? `Updated ${relativeTime(health.lastSuccessAt)}` : "No successful poll");
  setText("connector-name", health.connector || "Auto detect");
  setText("server-version", health.serverVersion || "Version unavailable");

  const banner = $("connection-banner");
  banner.classList.remove("demo-banner", "starting-banner");
  if (health.state === "ok") {
    banner.hidden = true;
    return;
  }
  banner.hidden = false;
  if (health.state === "demo") {
    banner.classList.add("demo-banner");
    setText("banner-title", "Demonstration data active");
  } else if (health.state === "starting") {
    banner.classList.add("starting-banner");
    setText("banner-title", "Connecting to Metasys");
  } else {
    setText("banner-title", "Metasys connection needs attention");
  }
  setText("banner-message", health.message || "Waiting for connector status");
}

function showClientError(message) {
  const banner = $("connection-banner");
  banner.hidden = false;
  banner.classList.remove("demo-banner", "starting-banner");
  setText("banner-title", "Dashboard service unavailable");
  setText("banner-message", message);
  const badge = $("health-badge");
  badge.className = "status-badge error";
  badge.lastChild.textContent = " Attention";
}

function renderActiveAlarms(alarms) {
  const body = $("active-alarms-body");
  body.replaceChildren();
  if (!alarms.length) return appendEmptyRow(body, 5, "No active alarms reported");
  const fragment = document.createDocumentFragment();
  alarms.forEach((alarm) => {
    const row = document.createElement("tr");
    row.append(
      tableCell(severityPill(alarm.severity)),
      tableCell(primarySecondary(alarm.equipment, alarm.point)),
      tableCell(primarySecondary(alarm.message, `${alarm.alarmType} · ${alarm.category}`)),
      tableCell(String(alarm.priority), "numeric"),
      tableCell(primarySecondary(relativeTime(alarm.occurredAt), formatTime(alarm.occurredAt)))
    );
    fragment.append(row);
  });
  body.append(fragment);
}

function renderOverrides(overrides) {
  const body = $("overrides-body");
  body.replaceChildren();
  if (!overrides.length) return appendEmptyRow(body, 3, "No active operator overrides");
  const fragment = document.createDocumentFragment();
  overrides.forEach((override) => {
    const row = document.createElement("tr");
    row.append(
      tableCell(primarySecondary(override.equipment, override.point)),
      tableCell(primarySecondary(override.value || "—", override.status)),
      tableCell(override.expiresAt ? relativeTime(override.expiresAt, true) : "Until released")
    );
    fragment.append(row);
  });
  body.append(fragment);
}

function renderFrequent(alarms) {
  const list = $("frequent-list");
  list.replaceChildren();
  if (!alarms.length) return appendEmpty(list, "No alarm history collected");
  alarms.slice(0, 10).forEach((alarm, index) => {
    const value = document.createElement("span");
    value.className = "rank-value";
    value.append(document.createTextNode(numberFormat.format(alarm.count || 0)), smallLabel("events"));
    list.append(rankItem(index, `${alarm.equipment} · ${alarm.point}`, `${alarm.alarmType} · latest ${relativeTime(alarm.occurredAt)}`, value));
  });
}

function renderSerious(alarms) {
  const list = $("serious-list");
  list.replaceChildren();
  if (!alarms.length) return appendEmpty(list, "No alarm history collected");
  alarms.slice(0, 10).forEach((alarm, index) => {
    const value = document.createElement("span");
    value.className = `rank-value risk-value ${alarm.severity}`;
    value.append(document.createTextNode(String(alarm.priority)), smallLabel(alarm.severity));
    list.append(rankItem(index, `${alarm.equipment} · ${alarm.point}`, `${alarm.message} · ${relativeTime(alarm.occurredAt)}`, value));
  });
}

function renderEquipment(equipment) {
  const list = $("equipment-list");
  list.replaceChildren();
  if (!equipment.length) return appendEmpty(list, "No equipment alarm history collected");
  const maxScore = Math.max(...equipment.map((item) => item.score), 1);
  equipment.slice(0, 10).forEach((item) => {
    const wrapper = document.createElement("div");
    wrapper.className = "equipment-item";
    const heading = document.createElement("div");
    heading.className = "equipment-name-row";
    const name = document.createElement("span");
    name.className = "equipment-name";
    name.textContent = item.equipment;
    const score = document.createElement("span");
    score.className = "equipment-score";
    score.textContent = item.score;
    heading.append(name, score);
    const bar = document.createElement("div");
    bar.className = "equipment-bar";
    const fill = document.createElement("span");
    fill.style.width = `${Math.max(3, item.score * 100 / maxScore)}%`;
    bar.append(fill);
    const stats = document.createElement("div");
    stats.className = "equipment-stats";
    stats.append(stat(`${numberFormat.format(item.alarmCount)}`, "alarms"), stat(`${item.activeCount}`, "active"), stat(`${item.percentage}%`, "of total"));
    wrapper.append(heading, bar, stats);
    list.append(wrapper);
  });
}

function drawCharts() {
  if (!dashboardData) return;
  drawLineChart($("alarm-rate-chart"), dashboardData.alarmRate);
  drawDonut($("type-chart"), $("type-legend"), dashboardData.alarmsByType, "alarms");
  drawDonut($("equipment-chart"), $("equipment-legend"), dashboardData.alarmsByEquipment, "alarms");
}

function drawLineChart(canvas, points) {
  const { ctx, width, height } = canvasContext(canvas);
  const margin = { top: 17, right: 18, bottom: 33, left: 38 };
  const plotWidth = Math.max(1, width - margin.left - margin.right);
  const plotHeight = Math.max(1, height - margin.top - margin.bottom);
  const maxValue = Math.max(4, ...points.map((point) => Math.max(point.count, point.rollingAverage)));
  const ceiling = Math.ceil(maxValue / 4) * 4;
  const x = (index) => margin.left + (points.length <= 1 ? 0 : index * plotWidth / (points.length - 1));
  const y = (value) => margin.top + plotHeight - value * plotHeight / ceiling;

  ctx.clearRect(0, 0, width, height);
  ctx.font = "10px -apple-system, sans-serif";
  ctx.textBaseline = "middle";
  for (let index = 0; index <= 4; index += 1) {
    const value = ceiling * index / 4;
    const lineY = y(value);
    ctx.beginPath();
    ctx.strokeStyle = "rgba(129, 165, 180, .11)";
    ctx.lineWidth = 1;
    ctx.moveTo(margin.left, lineY);
    ctx.lineTo(width - margin.right, lineY);
    ctx.stroke();
    ctx.fillStyle = "#58717c";
    ctx.textAlign = "right";
    ctx.fillText(String(Math.round(value)), margin.left - 9, lineY);
  }

  points.forEach((point, index) => {
    if (index % 2 !== 0 && index !== points.length - 1) return;
    ctx.fillStyle = "#58717c";
    ctx.textAlign = index === points.length - 1 ? "right" : "center";
    const date = new Date(`${point.date}T12:00:00`);
    ctx.fillText(date.toLocaleDateString(undefined, { month: "short", day: "numeric" }), x(index), height - 12);
  });

  const gradient = ctx.createLinearGradient(0, margin.top, 0, margin.top + plotHeight);
  gradient.addColorStop(0, "rgba(44, 199, 210, .26)");
  gradient.addColorStop(1, "rgba(44, 199, 210, 0)");
  ctx.beginPath();
  points.forEach((point, index) => index ? ctx.lineTo(x(index), y(point.count)) : ctx.moveTo(x(index), y(point.count)));
  if (points.length) {
    ctx.lineTo(x(points.length - 1), y(0));
    ctx.lineTo(x(0), y(0));
    ctx.closePath();
    ctx.fillStyle = gradient;
    ctx.fill();
  }

  drawSeries(ctx, points, x, (point) => y(point.count), "#2cc7d2", false);
  drawSeries(ctx, points, x, (point) => y(point.rollingAverage), "#f2b84b", true);
  points.forEach((point, index) => {
    ctx.beginPath();
    ctx.fillStyle = "#0d1c26";
    ctx.strokeStyle = "#2cc7d2";
    ctx.lineWidth = 1.5;
    ctx.arc(x(index), y(point.count), 2.7, 0, Math.PI * 2);
    ctx.fill();
    ctx.stroke();
  });
  canvas.setAttribute("aria-label", `Daily alarms for 14 days. Latest day: ${points.at(-1)?.count || 0} events.`);
}

function drawSeries(ctx, points, x, y, color, dashed) {
  if (!points.length) return;
  ctx.beginPath();
  ctx.strokeStyle = color;
  ctx.lineWidth = dashed ? 1.5 : 2;
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  ctx.setLineDash(dashed ? [5, 5] : []);
  points.forEach((point, index) => index ? ctx.lineTo(x(index), y(point)) : ctx.moveTo(x(index), y(point)));
  ctx.stroke();
  ctx.setLineDash([]);
}

function drawDonut(canvas, legend, slices, noun) {
  const { ctx, width, height } = canvasContext(canvas);
  ctx.clearRect(0, 0, width, height);
  const total = slices.reduce((sum, slice) => sum + slice.count, 0);
  const centerX = width / 2;
  const centerY = height / 2;
  const radius = Math.max(22, Math.min(width, height) * .35);
  const thickness = Math.max(11, radius * .26);
  let angle = -Math.PI / 2;

  if (!total) {
    ctx.beginPath();
    ctx.strokeStyle = "#1c323d";
    ctx.lineWidth = thickness;
    ctx.arc(centerX, centerY, radius, 0, Math.PI * 2);
    ctx.stroke();
  } else {
    slices.forEach((slice, index) => {
      const arc = slice.count / total * Math.PI * 2;
      ctx.beginPath();
      ctx.strokeStyle = palette[index % palette.length];
      ctx.lineWidth = thickness;
      ctx.lineCap = "butt";
      ctx.arc(centerX, centerY, radius, angle + .015, angle + arc - .015);
      ctx.stroke();
      angle += arc;
    });
  }
  ctx.fillStyle = "#edf5f7";
  ctx.font = "650 24px -apple-system, sans-serif";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillText(compactNumber(total), centerX, centerY - 5);
  ctx.fillStyle = "#607b87";
  ctx.font = "700 9px -apple-system, sans-serif";
  ctx.fillText(noun.toUpperCase(), centerX, centerY + 15);
  canvas.setAttribute("aria-label", `${numberFormat.format(total)} ${noun}: ${slices.map((slice) => `${slice.label} ${slice.percentage}%`).join(", ")}`);

  legend.replaceChildren();
  if (!slices.length) return appendEmpty(legend, "No history yet");
  slices.forEach((slice, index) => {
    const row = document.createElement("div");
    row.className = "legend-row";
    const color = document.createElement("i");
    color.className = "legend-color";
    color.style.background = palette[index % palette.length];
    const label = document.createElement("span");
    label.className = "legend-label";
    label.textContent = slice.label;
    const value = document.createElement("span");
    value.className = "legend-value";
    value.textContent = `${slice.percentage}%`;
    row.append(color, label, value);
    legend.append(row);
  });
}

function canvasContext(canvas) {
  const ratio = Math.min(window.devicePixelRatio || 1, 2);
  const width = Math.max(1, canvas.clientWidth);
  const height = Math.max(1, canvas.clientHeight);
  canvas.width = Math.round(width * ratio);
  canvas.height = Math.round(height * ratio);
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  return { ctx, width, height };
}

function severityPill(severity) {
  const span = document.createElement("span");
  span.className = `severity-pill ${severity}`;
  span.textContent = severity;
  return span;
}

function primarySecondary(primary, secondary) {
  const wrapper = document.createElement("span");
  const first = document.createElement("span");
  first.className = "cell-primary";
  first.textContent = primary || "—";
  const second = document.createElement("span");
  second.className = "cell-secondary";
  second.textContent = secondary || "—";
  wrapper.append(first, second);
  return wrapper;
}

function tableCell(content, className) {
  const cell = document.createElement("td");
  if (className) cell.className = className;
  cell.append(content instanceof Node ? content : document.createTextNode(content || "—"));
  return cell;
}

function appendEmptyRow(body, columns, message) {
  const row = document.createElement("tr");
  const cell = tableCell(message, "empty-cell");
  cell.colSpan = columns;
  row.append(cell);
  body.append(row);
}

function appendEmpty(container, message) {
  const text = document.createElement("p");
  text.className = "empty-state";
  text.textContent = message;
  container.append(text);
}

function rankItem(index, titleText, detailText, value) {
  const item = document.createElement("div");
  item.className = "rank-item";
  const number = document.createElement("span");
  number.className = "rank-number";
  number.textContent = String(index + 1).padStart(2, "0");
  const content = document.createElement("div");
  content.className = "rank-content";
  const title = document.createElement("span");
  title.className = "rank-title";
  title.textContent = titleText;
  const detail = document.createElement("span");
  detail.className = "rank-detail";
  detail.textContent = detailText;
  content.append(title, detail);
  item.append(number, content, value);
  return item;
}

function smallLabel(text) {
  const small = document.createElement("small");
  small.textContent = text;
  return small;
}

function stat(value, label) {
  const span = document.createElement("span");
  const bold = document.createElement("b");
  bold.textContent = value;
  span.append(bold, document.createTextNode(` ${label}`));
  return span;
}

function setText(id, value) { $(id).textContent = value; }
function wait(milliseconds) { return new Promise((resolve) => window.setTimeout(resolve, milliseconds)); }

function formatTime(value) {
  if (!value) return "Never";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Unknown";
  return date.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
}

function relativeTime(value, futureOnly = false) {
  if (!value) return "Never";
  const timestamp = new Date(value).getTime();
  if (Number.isNaN(timestamp)) return "Unknown";
  const seconds = Math.round((timestamp - Date.now()) / 1000);
  const absolute = Math.abs(seconds);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  let amount;
  let unit;
  if (absolute < 60) { amount = seconds; unit = "second"; }
  else if (absolute < 3600) { amount = Math.round(seconds / 60); unit = "minute"; }
  else if (absolute < 86400) { amount = Math.round(seconds / 3600); unit = "hour"; }
  else { amount = Math.round(seconds / 86400); unit = "day"; }
  const output = formatter.format(amount, unit);
  return futureOnly && seconds > 0 ? output : output;
}

function historyCoverage(value) {
  if (!value) return "History collection starts after first poll";
  const days = Math.max(1, Math.min(30, Math.ceil((Date.now() - new Date(value).getTime()) / 86_400_000)));
  return `${days}-day indexed history`;
}

function compactNumber(value) {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value);
}
