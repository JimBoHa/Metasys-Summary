"use strict";

const COLORS = ["#2cc7d2", "#f2b84b", "#9a8cff", "#53d18b", "#ef7278", "#62a8e5", "#ef9f62", "#d27bc5"];
const MAX_RENDERED_POINTS = 300;
const state = {
  session: null,
  points: [],
  selected: new Map(),
  response: null,
  rangeHours: 168,
  customRange: false,
  zoom: null,
  dragStartX: null,
  layout: null,
  tableRendered: false
};

const $ = (id) => document.getElementById(id);

document.addEventListener("DOMContentLoaded", async () => {
  bindEvents();
  setDefaultCustomRange();
  try {
    const session = await fetchJson("/api/portal/me");
    if (!['admin', 'operator'].includes(session.user.role)) {
      window.location.assign("/");
      return;
    }
    state.session = session;
    $("user-name").textContent = session.user.displayName;
    $("user-role").textContent = roleLabel(session.user.role);
    await loadPointCatalog();
  } catch (error) {
    showMessage(error.message || "Unable to open the trend workspace", true);
  }
});

function bindEvents() {
  $("point-search").addEventListener("input", renderPointCatalog);
  $("reload-points-button").addEventListener("click", loadPointCatalog);
  $("load-trends-button").addEventListener("click", loadTrends);
  $("display-mode").addEventListener("change", () => {
    state.zoom = null;
    renderAnalysis();
  });
  $("smoothing-window").addEventListener("change", renderChart);
  $("trend-interval").addEventListener("change", updateIntervalRecommendation);
  $("range-start").addEventListener("change", updateIntervalRecommendation);
  $("range-end").addEventListener("change", updateIntervalRecommendation);
  $("reset-zoom-button").addEventListener("click", () => {
    state.zoom = null;
    $("reset-zoom-button").disabled = true;
    renderChart();
  });
  $("toggle-table-button").addEventListener("click", toggleDataTable);
  $("export-csv-button").addEventListener("click", exportCsv);
  $("sign-out-button").addEventListener("click", signOut);
  document.querySelectorAll("#range-presets button").forEach((button) => {
    button.addEventListener("click", () => selectRange(button));
  });

  const chart = $("trend-chart");
  chart.addEventListener("pointerdown", beginZoom);
  chart.addEventListener("pointermove", moveChartPointer);
  chart.addEventListener("pointerup", endZoom);
  chart.addEventListener("pointercancel", cancelZoom);
  chart.addEventListener("pointerleave", () => {
    if (state.dragStartX === null) $("chart-tooltip").hidden = true;
  });
  new ResizeObserver(renderChart).observe($("chart-stage"));
}

async function loadPointCatalog() {
  const button = $("reload-points-button");
  button.disabled = true;
  $("point-results").replaceChildren(element("p", "empty-state", "Loading historian catalog…"));
  try {
    const catalog = await fetchJson("/api/trend-points");
    state.points = catalog.points || [];
    $("catalog-count").textContent = `${formatInteger(state.points.length)} point${state.points.length === 1 ? "" : "s"}${catalog.truncated ? "+" : ""}`;
    renderPointCatalog();
    showMessage(catalog.truncated ? "The point catalog reached its 10,000-point display limit. Refine the source database if a point is missing." : "");
  } catch (error) {
    state.points = [];
    $("catalog-count").textContent = "Unavailable";
    $("point-results").replaceChildren(element("p", "empty-state", error.message || "Unable to load historian points"));
    showMessage(error.message || "Unable to load historian points", true);
  } finally {
    button.disabled = false;
  }
}

function renderPointCatalog() {
  const query = $("point-search").value.trim().toLocaleLowerCase();
  const matches = state.points.filter((point) => {
    if (!query) return true;
    return `${point.pointName} ${point.unit || ""}`.toLocaleLowerCase().includes(query);
  });
  const visible = matches.slice(0, MAX_RENDERED_POINTS);
  const fragment = document.createDocumentFragment();
  for (const point of visible) {
    const selected = state.selected.has(point.pointSliceId);
    const row = element("button", `point-row${selected ? " selected" : ""}`);
    row.type = "button";
    row.dataset.pointId = String(point.pointSliceId);
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", String(selected));
    row.title = point.pointName;
    const check = element("span", "point-check", selected ? "✓" : "");
    check.setAttribute("aria-hidden", "true");
    const copy = element("span", "point-copy");
    copy.append(element("strong", "", point.pointName), element("small", "", `${point.unit || "No unit"} · PointSlice ${point.pointSliceId}`));
    row.append(check, copy);
    row.addEventListener("click", () => togglePoint(point));
    fragment.append(row);
  }
  if (!visible.length) fragment.append(element("p", "empty-state", state.points.length ? "No points match this search." : "No historian points are available."));
  $("point-results").replaceChildren(fragment);
  $("result-count").textContent = matches.length > MAX_RENDERED_POINTS ? `${MAX_RENDERED_POINTS} of ${formatInteger(matches.length)} shown` : `${formatInteger(matches.length)} shown`;
}

function togglePoint(point) {
  if (state.selected.has(point.pointSliceId)) {
    state.selected.delete(point.pointSliceId);
  } else {
    if (state.selected.size >= 8) {
      showMessage("Select no more than eight historian points.", true);
      return;
    }
    state.selected.set(point.pointSliceId, point);
    showMessage("");
  }
  renderSelectedPoints();
  renderPointCatalog();
  updateIntervalRecommendation();
}

function renderSelectedPoints() {
  const container = $("selected-points");
  if (!state.selected.size) {
    container.replaceChildren(element("p", "", "Select one or more points below."));
    return;
  }
  const fragment = document.createDocumentFragment();
  for (const point of state.selected.values()) {
    const chip = element("span", "point-chip");
    chip.title = point.pointName;
    chip.append(element("span", "", point.pointName));
    const remove = element("button", "", "×");
    remove.type = "button";
    remove.setAttribute("aria-label", `Remove ${point.pointName}`);
    remove.addEventListener("click", () => togglePoint(point));
    chip.append(remove);
    fragment.append(chip);
  }
  container.replaceChildren(fragment);
}

function selectRange(button) {
  document.querySelectorAll("#range-presets button").forEach((candidate) => candidate.classList.toggle("active", candidate === button));
  state.customRange = button.dataset.hours === "custom";
  state.rangeHours = state.customRange ? null : Number(button.dataset.hours);
  $("custom-range").hidden = !state.customRange;
  updateIntervalRecommendation();
}

async function loadTrends() {
  if (!state.selected.size) {
    showMessage("Select at least one historian point before graphing.", true);
    return;
  }
  const params = new URLSearchParams();
  params.set("pointSlices", [...state.selected.keys()].join(","));
  if (state.customRange) {
    const start = dateFromInput($("range-start").value);
    const end = dateFromInput($("range-end").value);
    if (!start || !end || start >= end) {
      showMessage("Choose a valid custom start and end time.", true);
      return;
    }
    params.set("from", start.toISOString());
    params.set("to", end.toISOString());
  } else {
    params.set("hours", String(state.rangeHours));
  }
  if ($("trend-interval").value) params.set("intervalSeconds", $("trend-interval").value);

  const button = $("load-trends-button");
  button.disabled = true;
  button.textContent = "Querying SQL…";
  showMessage("Reading selected trend data from the remote SQL historian…");
  try {
    state.response = await fetchJson(`/api/trends?${params}`);
    state.zoom = null;
    state.tableRendered = false;
    $("data-table-wrap").hidden = true;
    $("toggle-table-button").textContent = "Show data table";
    renderAnalysis();
    const warning = state.response.truncated ? " The response reached the 5,000-row safety limit." : "";
    showMessage(`Loaded ${formatInteger(state.response.sampleCount)} samples across ${state.response.series.length} series.${warning}`, state.response.truncated);
  } catch (error) {
    showMessage(error.message || "Unable to query historian trends", true);
  } finally {
    button.disabled = false;
    button.textContent = "Graph selected points";
  }
}

function renderAnalysis() {
  const hasData = Boolean(state.response?.series?.some((series) => series.samples.length));
  $("chart-placeholder").hidden = hasData;
  $("reset-zoom-button").disabled = !state.zoom;
  $("toggle-table-button").disabled = !hasData;
  $("export-csv-button").disabled = !hasData;
  renderQuerySummary();
  renderLegend();
  renderStatistics();
  renderChart();
  if (state.tableRendered) renderDataTable();
}

function renderQuerySummary() {
  if (!state.response) {
    $("query-summary").textContent = "Select points and run a query.";
    return;
  }
  const from = formatDateTime(state.response.from);
  const to = formatDateTime(state.response.to);
  const bucket = state.response.bucketSeconds ? `${capitalize(state.response.aggregation || "aggregated")} values in ${formatDuration(state.response.bucketSeconds)} buckets` : "Advanced read-only query";
  $("query-summary").textContent = `${from} – ${to} · ${bucket} · ${formatInteger(state.response.sampleCount)} samples`;
}

function renderLegend() {
  const container = $("chart-legend");
  const fragment = document.createDocumentFragment();
  for (const [index, series] of (state.response?.series || []).entries()) {
    const item = element("span", "legend-item");
    const swatch = element("i");
    swatch.style.background = COLORS[index % COLORS.length];
    item.append(swatch, element("span", "", series.name), element("small", "", series.unit || "No unit"));
    item.title = series.name;
    fragment.append(item);
  }
  container.replaceChildren(fragment);
}

function renderStatistics() {
  const container = $("statistics-grid");
  if (!state.response?.series?.length) {
    container.replaceChildren(element("p", "empty-state", "Statistics appear after a query."));
    return;
  }
  const fragment = document.createDocumentFragment();
  state.response.series.forEach((series, index) => {
    const card = element("article", "stat-series");
    const header = document.createElement("header");
    const marker = element("i");
    marker.style.background = COLORS[index % COLORS.length];
    const title = document.createElement("div");
    title.append(element("h3", "", series.name), element("small", "", `${formatInteger(series.statistics.count)} samples · ${series.unit || "No unit"}`));
    header.append(marker, title);
    const values = element("div", "stat-values");
    const unit = series.unit || "";
    addStatistic(values, "Latest", formatValue(series.statistics.latest, unit));
    addStatistic(values, "Average", formatValue(series.statistics.average, unit));
    addStatistic(values, "Range", `${formatNumber(series.statistics.minimum)} – ${formatNumber(series.statistics.maximum)}${unit ? ` ${unit}` : ""}`);
    addStatistic(values, "Change", signedValue(series.statistics.change, unit));
    addStatistic(values, "Linear rate", signedValue(series.statistics.ratePerDay, unit ? `${unit}/day` : "/day"));
    addStatistic(values, "Samples", formatInteger(series.statistics.count));
    card.append(header, values);
    fragment.append(card);
  });
  container.replaceChildren(fragment);
}

function addStatistic(container, label, value) {
  const item = document.createElement("div");
  item.append(element("span", "", label), element("strong", "", value));
  container.append(item);
}

function processedSeries() {
  if (!state.response) return [];
  const windowSize = Number($("smoothing-window").value || 1);
  const normalized = $("display-mode").value === "normalized";
  return state.response.series.map((series, index) => {
    const smoothed = movingMean(series.samples, windowSize);
    const base = smoothed.find((sample) => Number.isFinite(sample.value) && Math.abs(sample.value) > 1e-12)?.value;
    const samples = normalized && base !== undefined
      ? smoothed.map((sample) => ({ timestamp: sample.timestamp, value: ((sample.value - base) / Math.abs(base)) * 100 }))
      : smoothed;
    return {
      name: series.name,
      unit: normalized ? "% change" : (series.unit || "Value"),
      color: COLORS[index % COLORS.length],
      samples
    };
  });
}

function movingMean(samples, windowSize) {
  if (windowSize <= 1) return samples.map((sample) => ({ timestamp: sample.timestamp, value: sample.value }));
  const radius = Math.floor(windowSize / 2);
  return samples.map((sample, index) => {
    const start = Math.max(0, index - radius);
    const end = Math.min(samples.length, index + radius + 1);
    const window = samples.slice(start, end);
    return { timestamp: sample.timestamp, value: window.reduce((sum, item) => sum + item.value, 0) / window.length };
  });
}

function renderChart() {
  const canvas = $("trend-chart");
  const rect = canvas.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  const ratio = Math.max(1, window.devicePixelRatio || 1);
  canvas.width = Math.round(rect.width * ratio);
  canvas.height = Math.round(rect.height * ratio);
  const context = canvas.getContext("2d");
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, rect.width, rect.height);
  const allSeries = processedSeries();
  if (!allSeries.some((series) => series.samples.length)) {
    state.layout = null;
    return;
  }

  const responseFrom = new Date(state.response.from).getTime();
  const responseTo = new Date(state.response.to).getTime();
  const from = state.zoom?.from ?? responseFrom;
  const to = state.zoom?.to ?? responseTo;
  const units = [...new Set(allSeries.map((series) => series.unit))];
  const plot = { left: 68, right: rect.width - (units.length > 1 ? 72 : 24), top: 22, bottom: rect.height - 42 };
  if (plot.right <= plot.left || plot.bottom <= plot.top) return;
  const scales = calculateScales(allSeries, units, from, to);
  state.layout = { plot, from, to, width: rect.width, height: rect.height, series: allSeries, scales };

  context.font = "9px -apple-system, BlinkMacSystemFont, sans-serif";
  context.lineWidth = 1;
  drawGrid(context, plot, from, to, units, scales);
  context.save();
  context.beginPath();
  context.rect(plot.left, plot.top, plot.right - plot.left, plot.bottom - plot.top);
  context.clip();
  for (const series of allSeries) drawSeries(context, series, scales.get(series.unit), plot, from, to);
  context.restore();
}

function calculateScales(seriesList, units, from, to) {
  const scales = new Map();
  for (const unit of units) {
    const values = seriesList
      .filter((series) => series.unit === unit)
      .flatMap((series) => series.samples.filter((sample) => inRange(sample.timestamp, from, to)).map((sample) => sample.value))
      .filter(Number.isFinite);
    let minimum = values.length ? Math.min(...values) : 0;
    let maximum = values.length ? Math.max(...values) : 1;
    if (minimum === maximum) {
      const padding = Math.max(Math.abs(minimum) * .05, 1);
      minimum -= padding;
      maximum += padding;
    } else {
      const padding = (maximum - minimum) * .08;
      minimum -= padding;
      maximum += padding;
    }
    scales.set(unit, { minimum, maximum });
  }
  return scales;
}

function drawGrid(context, plot, from, to, units, scales) {
  context.textBaseline = "middle";
  for (let index = 0; index <= 5; index += 1) {
    const ratio = index / 5;
    const y = plot.bottom - ratio * (plot.bottom - plot.top);
    context.strokeStyle = "rgba(117,153,170,.13)";
    context.beginPath();
    context.moveTo(plot.left, y);
    context.lineTo(plot.right, y);
    context.stroke();
    const primary = scales.get(units[0]);
    context.fillStyle = "#69828e";
    context.textAlign = "right";
    context.fillText(formatNumber(primary.minimum + ratio * (primary.maximum - primary.minimum)), plot.left - 9, y);
    if (units.length > 1) {
      const secondary = scales.get(units[1]);
      context.textAlign = "left";
      context.fillText(formatNumber(secondary.minimum + ratio * (secondary.maximum - secondary.minimum)), plot.right + 9, y);
    }
  }
  for (let index = 0; index <= 5; index += 1) {
    const ratio = index / 5;
    const x = plot.left + ratio * (plot.right - plot.left);
    context.strokeStyle = "rgba(117,153,170,.08)";
    context.beginPath();
    context.moveTo(x, plot.top);
    context.lineTo(x, plot.bottom);
    context.stroke();
    context.fillStyle = "#69828e";
    context.textAlign = index === 0 ? "left" : index === 5 ? "right" : "center";
    context.textBaseline = "top";
    context.fillText(formatAxisTime(from + ratio * (to - from), to - from), x, plot.bottom + 10);
  }
  context.save();
  context.fillStyle = "#78919c";
  context.font = "8px -apple-system, BlinkMacSystemFont, sans-serif";
  context.textBaseline = "top";
  context.textAlign = "left";
  context.fillText(units[0], plot.left, 5);
  if (units.length > 1) {
    context.textAlign = "right";
    const suffix = units.length > 2 ? ` + ${units.length - 2} independent scale${units.length === 3 ? "" : "s"}` : "";
    context.fillText(`${units[1]}${suffix}`, plot.right, 5);
  }
  context.restore();
}

function drawSeries(context, series, scale, plot, from, to) {
  const visible = series.samples.filter((sample) => inRange(sample.timestamp, from, to));
  if (!visible.length) return;
  const gaps = visible.slice(1).map((sample, index) => new Date(sample.timestamp).getTime() - new Date(visible[index].timestamp).getTime()).filter((gap) => gap > 0).sort((a, b) => a - b);
  const medianGap = gaps.length ? gaps[Math.floor(gaps.length / 2)] : Infinity;
  const gapThreshold = Number.isFinite(medianGap) ? medianGap * 4 : Infinity;
  context.strokeStyle = series.color;
  context.lineWidth = 1.7;
  context.lineJoin = "round";
  context.lineCap = "round";
  context.beginPath();
  let priorTime = null;
  visible.forEach((sample) => {
    const time = new Date(sample.timestamp).getTime();
    const x = plot.left + ((time - from) / (to - from)) * (plot.right - plot.left);
    const y = plot.bottom - ((sample.value - scale.minimum) / (scale.maximum - scale.minimum)) * (plot.bottom - plot.top);
    if (priorTime === null || time - priorTime > gapThreshold) context.moveTo(x, y);
    else context.lineTo(x, y);
    priorTime = time;
  });
  context.stroke();
}

function beginZoom(event) {
  if (!state.layout) return;
  const x = chartX(event);
  if (x < state.layout.plot.left || x > state.layout.plot.right) return;
  state.dragStartX = x;
  $("trend-chart").setPointerCapture(event.pointerId);
  $("chart-tooltip").hidden = true;
}

function moveChartPointer(event) {
  if (!state.layout) return;
  const x = chartX(event);
  if (state.dragStartX !== null) {
    const left = Math.max(state.layout.plot.left, Math.min(state.dragStartX, x));
    const right = Math.min(state.layout.plot.right, Math.max(state.dragStartX, x));
    const selection = $("zoom-selection");
    selection.hidden = false;
    selection.style.left = `${left}px`;
    selection.style.width = `${Math.max(0, right - left)}px`;
    return;
  }
  showTooltip(event, x);
}

function endZoom(event) {
  if (state.dragStartX === null || !state.layout) return;
  const startX = state.dragStartX;
  const endX = Math.max(state.layout.plot.left, Math.min(state.layout.plot.right, chartX(event)));
  state.dragStartX = null;
  $("zoom-selection").hidden = true;
  if (Math.abs(endX - startX) < 12) return;
  const left = Math.min(startX, endX);
  const right = Math.max(startX, endX);
  const span = state.layout.to - state.layout.from;
  const plotWidth = state.layout.plot.right - state.layout.plot.left;
  state.zoom = {
    from: state.layout.from + ((left - state.layout.plot.left) / plotWidth) * span,
    to: state.layout.from + ((right - state.layout.plot.left) / plotWidth) * span
  };
  $("reset-zoom-button").disabled = false;
  renderChart();
}

function cancelZoom() {
  state.dragStartX = null;
  $("zoom-selection").hidden = true;
}

function showTooltip(event, x) {
  const { plot, from, to, series } = state.layout;
  const y = event.clientY - $("trend-chart").getBoundingClientRect().top;
  if (x < plot.left || x > plot.right || y < plot.top || y > plot.bottom) {
    $("chart-tooltip").hidden = true;
    return;
  }
  const target = from + ((x - plot.left) / (plot.right - plot.left)) * (to - from);
  const rows = series.map((item) => ({ series: item, sample: nearestSample(item.samples, target, from, to) })).filter((item) => item.sample);
  if (!rows.length) return;
  const tooltip = $("chart-tooltip");
  const content = document.createDocumentFragment();
  content.append(element("strong", "", formatDateTime(new Date(target).toISOString())));
  for (const row of rows) {
    const line = element("div", "tooltip-row");
    const marker = element("i");
    marker.style.background = row.series.color;
    line.append(marker, element("span", "", row.series.name), element("b", "", `${formatNumber(row.sample.value)} ${row.series.unit}`));
    content.append(line);
  }
  tooltip.replaceChildren(content);
  tooltip.hidden = false;
  const stageRect = $("chart-stage").getBoundingClientRect();
  const proposedLeft = x + 14;
  tooltip.style.left = `${Math.min(proposedLeft, stageRect.width - tooltip.offsetWidth - 8)}px`;
  tooltip.style.top = `${Math.max(8, Math.min(y + 12, stageRect.height - tooltip.offsetHeight - 8))}px`;
}

function nearestSample(samples, target, from, to) {
  const visible = samples.filter((sample) => inRange(sample.timestamp, from, to));
  if (!visible.length) return null;
  let low = 0;
  let high = visible.length - 1;
  while (low < high) {
    const middle = Math.floor((low + high) / 2);
    if (new Date(visible[middle].timestamp).getTime() < target) low = middle + 1;
    else high = middle;
  }
  const before = visible[Math.max(0, low - 1)];
  const after = visible[low];
  return Math.abs(new Date(before.timestamp).getTime() - target) <= Math.abs(new Date(after.timestamp).getTime() - target) ? before : after;
}

function toggleDataTable() {
  const wrap = $("data-table-wrap");
  wrap.hidden = !wrap.hidden;
  $("toggle-table-button").textContent = wrap.hidden ? "Show data table" : "Hide data table";
  if (!wrap.hidden && !state.tableRendered) renderDataTable();
}

function renderDataTable() {
  const rows = rawRows();
  const fragment = document.createDocumentFragment();
  for (const row of rows) {
    const tableRow = document.createElement("tr");
    tableRow.append(
      element("td", "", formatDateTime(row.timestamp)),
      element("td", "", row.name),
      element("td", "", formatNumber(row.value)),
      element("td", "", row.unit || "")
    );
    fragment.append(tableRow);
  }
  $("data-table-body").replaceChildren(fragment);
  state.tableRendered = true;
}

function exportCsv() {
  if (!state.response) return;
  const bucket = state.response.bucketSeconds || "";
  const aggregation = state.response.aggregation || "custom query";
  const lines = [["timestamp_utc", "point_name", "value", "unit", "bucket_seconds", "aggregation"]];
  rawRows().forEach((row) => lines.push([row.timestamp, row.name, row.value, row.unit || "", bucket, aggregation]));
  const csv = lines.map((row) => row.map(csvCell).join(",")).join("\r\n");
  const blob = new Blob([csv], { type: "text/csv;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = `metasys-trends-${new Date().toISOString().slice(0, 10)}.csv`;
  document.body.append(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

function rawRows() {
  return (state.response?.series || []).flatMap((series) => series.samples.map((sample) => ({
    timestamp: sample.timestamp,
    name: series.name,
    value: sample.value,
    unit: series.unit
  }))).sort((a, b) => new Date(a.timestamp) - new Date(b.timestamp) || a.name.localeCompare(b.name));
}

function updateIntervalRecommendation() {
  if (!state.selected.size) {
    $("interval-recommendation").textContent = "Automatic protects the 5,000-row response limit.";
    return;
  }
  const seconds = selectedRangeSeconds();
  if (!seconds) return;
  const bucketsPerSeries = Math.max(1, Math.floor(5000 / state.selected.size) - 1);
  const minimum = Math.max(1, Math.ceil(seconds / bucketsPerSeries));
  const requested = Number($("trend-interval").value || 0);
  $("interval-recommendation").textContent = requested && requested < minimum
    ? `The server will use at least ${formatDuration(minimum)} to stay within 5,000 rows.`
    : `Automatic recommendation: ${formatDuration(minimum)} or coarser.`;
}

function selectedRangeSeconds() {
  if (!state.customRange) return state.rangeHours * 3600;
  const start = dateFromInput($("range-start").value);
  const end = dateFromInput($("range-end").value);
  return start && end && end > start ? Math.floor((end - start) / 1000) : null;
}

function setDefaultCustomRange() {
  const end = new Date();
  const start = new Date(end.getTime() - 7 * 24 * 3600 * 1000);
  $("range-start").value = localDateTimeValue(start);
  $("range-end").value = localDateTimeValue(end);
}

async function signOut() {
  try {
    await fetchJson("/api/portal/logout", { method: "POST" });
  } finally {
    window.location.assign("/");
  }
}

async function fetchJson(url, options = {}) {
  const headers = new Headers(options.headers || {});
  if (options.method && !["GET", "HEAD", "OPTIONS"].includes(options.method) && state.session?.csrfToken) headers.set("X-CSRF-Token", state.session.csrfToken);
  const response = await fetch(url, { ...options, headers, cache: "no-store", credentials: "same-origin" });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    if (response.status === 401) window.location.assign("/");
    throw Object.assign(new Error(body.error || `Request failed (${response.status})`), { status: response.status });
  }
  return body;
}

function showMessage(message, error = false) {
  const target = $("page-message");
  target.hidden = !message;
  target.textContent = message || "";
  target.classList.toggle("error", Boolean(error));
}

function element(tag, className = "", text = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== "") node.textContent = text;
  return node;
}

function chartX(event) {
  return event.clientX - $("trend-chart").getBoundingClientRect().left;
}

function inRange(timestamp, from, to) {
  const time = new Date(timestamp).getTime();
  return time >= from && time <= to;
}

function dateFromInput(value) {
  if (!value) return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

function localDateTimeValue(date) {
  const offset = date.getTimezoneOffset() * 60_000;
  return new Date(date.getTime() - offset).toISOString().slice(0, 16);
}

function formatDateTime(value) {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "Unknown time" : new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(date);
}

function formatAxisTime(value, span) {
  const options = span <= 2 * 24 * 3600 * 1000 ? { hour: "numeric", minute: "2-digit" } : span <= 120 * 24 * 3600 * 1000 ? { month: "short", day: "numeric" } : { month: "short", year: "2-digit" };
  return new Intl.DateTimeFormat(undefined, options).format(new Date(value));
}

function formatDuration(seconds) {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.ceil(seconds / 60)}m`;
  if (seconds < 86400) return `${roundCompact(seconds / 3600)}h`;
  return `${roundCompact(seconds / 86400)}d`;
}

function formatNumber(value) {
  if (value === null || value === undefined || !Number.isFinite(Number(value))) return "—";
  const number = Number(value);
  const magnitude = Math.abs(number);
  const maximumFractionDigits = magnitude >= 100 ? 1 : magnitude >= 10 ? 2 : 3;
  return new Intl.NumberFormat(undefined, { maximumFractionDigits }).format(number);
}

function formatValue(value, unit) {
  const formatted = formatNumber(value);
  return formatted === "—" || !unit ? formatted : `${formatted} ${unit}`;
}

function signedValue(value, unit) {
  if (value === null || value === undefined || !Number.isFinite(Number(value))) return "—";
  const prefix = Number(value) > 0 ? "+" : "";
  return `${prefix}${formatNumber(value)}${unit ? ` ${unit}` : ""}`;
}

function formatInteger(value) {
  return new Intl.NumberFormat().format(Number(value || 0));
}

function roundCompact(value) {
  return Number(value.toFixed(value >= 10 ? 0 : 1));
}

function roleLabel(role) {
  return role === "admin" ? "Administrator" : role === "operator" ? "Operator" : role;
}

function capitalize(value) {
  return value ? value.charAt(0).toUpperCase() + value.slice(1) : "";
}

function csvCell(value) {
  const text = String(value ?? "");
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}
