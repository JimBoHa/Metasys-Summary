import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const repositoryDirectory = path.resolve(scriptDirectory, "..");
const staticDirectory = path.join(repositoryDirectory, "static");
const pageContracts = [
  { html: "portal.html", scripts: ["portal.js"] },
  { html: "index.html", scripts: ["app.js"] },
  { html: "trends.html", scripts: ["trends.js"] },
  { html: "diagnostics.html", scripts: ["diagnostics.js"] }
];
const errors = [];

function readStatic(file) {
  return fs.readFileSync(path.join(staticDirectory, file), "utf8");
}

function matches(source, expression) {
  return [...source.matchAll(expression)].map((match) => match[1]);
}

for (const contract of pageContracts) {
  const html = readStatic(contract.html);
  const ids = matches(html, /\bid="([^"]+)"/g);
  const uniqueIds = new Set(ids);
  for (const id of ids) {
    if (ids.indexOf(id) !== ids.lastIndexOf(id)) errors.push(`${contract.html}: duplicate id '${id}'`);
  }

  for (const script of contract.scripts) {
    const source = readStatic(script);
    const referencedIds = matches(source, /\$\(\s*"([^"]+)"\s*\)/g);
    for (const id of new Set(referencedIds)) {
      if (!uniqueIds.has(id)) errors.push(`${script}: references missing ${contract.html} id '${id}'`);
    }
  }

  const localAssets = matches(html, /\b(?:src|href)="\/([^"?#]+\.(?:js|css))"/g);
  for (const asset of localAssets) {
    if (!fs.existsSync(path.join(staticDirectory, asset))) errors.push(`${contract.html}: local asset '/${asset}' does not exist`);
  }
  for (const requiredAsset of ["/navigation.css", "/navigation.js"]) {
    if (!html.includes(`"${requiredAsset}"`)) errors.push(`${contract.html}: missing shared asset ${requiredAsset}`);
  }
  if (/<script(?![^>]*\bsrc=)[^>]*>/i.test(html)) errors.push(`${contract.html}: inline script violates the CSP contract`);
  if (/\son[a-z]+\s*=/i.test(html)) errors.push(`${contract.html}: inline event handler violates the CSP contract`);
  if (/unsafe-(?:inline|eval)/i.test(html)) errors.push(`${contract.html}: unsafe CSP directive found`);
}

for (const file of fs.readdirSync(staticDirectory).filter((name) => name.endsWith(".js"))) {
  const source = readStatic(file);
  if (/\b(?:innerHTML|outerHTML|insertAdjacentHTML|document\.write)\b|\beval\s*\(|new\s+Function\b/.test(source)) {
    errors.push(`${file}: unsafe DOM/code execution sink found`);
  }
}

const navigationKeys = pageContracts.map(({ html }) => {
  const keys = matches(readStatic(html), /\bdata-nav-key="([^"]+)"/g);
  return { html, keys: [...new Set(keys)].sort() };
});
const expectedKeys = JSON.stringify(navigationKeys[0].keys);
for (const navigation of navigationKeys.slice(1)) {
  if (JSON.stringify(navigation.keys) !== expectedKeys) errors.push(`${navigation.html}: sidebar destinations differ from ${navigationKeys[0].html}`);
}

if (errors.length) {
  for (const error of errors) process.stderr.write(`Static contract failed: ${error}\n`);
  process.exit(1);
}

process.stdout.write(`Static contracts passed for ${pageContracts.length} pages.\n`);
