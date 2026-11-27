// Run from the repository root after npm ci. The target and feature match the release build.
import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

const target = 'aarch64-apple-darwin';
const manifest = [
  '--manifest-path',
  'src-tauri/Cargo.toml',
  '--locked',
  '--features',
  'tauri/custom-protocol',
];
const run = (tool, args) =>
  execFileSync(tool, args, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
const read = (path) => readFileSync(path, 'utf8');
const json = (path) => JSON.parse(read(path));
const metadata = JSON.parse(
  run('cargo', [
    'metadata',
    ...manifest,
    '--format-version',
    '1',
    '--filter-platform',
    target,
  ]),
);
const tree = run('cargo', [
  'tree',
  ...manifest,
  '--target',
  target,
  '--edges',
  'normal',
  '--prefix',
  'none',
  '--format',
  '{p}',
]);
const resolved = new Set(
  tree
    .trim()
    .split('\n')
    .map((line) => line.split(' ').slice(0, 2).join(' ')),
);
const crates = metadata.packages.filter(
  (pkg) => pkg.source && resolved.has(pkg.name + ' v' + pkg.version),
);

// Keep the original attribution and terms. Many crates ship identical files.
const texts = new Map();
const index = [];
function register(name, version, license, url, documents) {
  if (!license || !documents.length)
    throw new Error('Missing license for ' + name);
  const refs = documents.map((document) => {
    const content = document
      .replaceAll('\r\n', '\n')
      .replace(/[ \t]+$/gm, '')
      .trim();
    if (!content) throw new Error('Empty notice for ' + name);
    if (!texts.has(content)) texts.set(content, texts.size + 1);
    return texts.get(content);
  });
  index.push(
    [
      name + ' ' + version,
      license,
      url,
      'License texts: ' + [...new Set(refs)].join(', '),
    ].join('\n'),
  );
}
function packagedTexts(directory) {
  return readdirSync(directory, { recursive: true, withFileTypes: true })
    .filter(
      (entry) =>
        entry.isFile() &&
        /^(licen[cs]es?|copying|notice|copyright|unlicense)(?:[._-]|$)/i.test(
          entry.name,
        ) &&
        !/\.(rs|js|json|ts)$/.test(entry.name),
    )
    .map((entry) => join(entry.parentPath, entry.name))
    .sort()
    .map(read);
}
const compare = (a, b) =>
  a.name < b.name
    ? -1
    : a.name > b.name
      ? 1
      : a.version.localeCompare(b.version, 'en');
const npmDirectories = run('npm', ['ls', '--omit=dev', '--all', '--parseable'])
  .trim()
  .split('\n')
  .slice(1);
// Tailwind's CSS is shipped even though its compiler is a development dependency.
npmDirectories.push('node_modules/tailwindcss');
for (const { pkg, directory } of npmDirectories
  .map((directory) => ({
    pkg: json(join(directory, 'package.json')),
    directory,
  }))
  .sort((a, b) => compare(a.pkg, b.pkg))) {
  const docs = packagedTexts(directory);
  if (pkg.name === '@tauri-apps/plugin-dialog') {
    const native = crates.find((crate) => crate.name === 'tauri-plugin-dialog');
    docs.push(...packagedTexts(dirname(native.manifest_path)));
  }
  register(
    pkg.name,
    pkg.version,
    pkg.license,
    'https://www.npmjs.com/package/' + pkg.name + '/v/' + pkg.version,
    docs,
  );
}

// These workspaces publish some crates without the repository-level license.
// Read that document at the revision recorded inside each crates.io archive.
const upstreamFiles = new Map([
  ['https://github.com/madsmtm/objc2', 'LICENSE.md'],
  ['https://github.com/open-i18n/rust-unic', 'LICENSE-MIT'],
  ['https://github.com/dropbox/rust-alloc-no-stdlib', 'LICENSE'],
]);
const upstreamCache = new Map();
for (const crate of crates.sort(compare)) {
  const directory = dirname(crate.manifest_path);
  const source =
    'https://crates.io/api/v1/crates/' +
    crate.name +
    '/' +
    crate.version +
    '/download';
  const docs = packagedTexts(directory);
  if (!docs.length) {
    const repo = crate.repository?.replace(/\/$/, '');
    if (crate.name === 'selectors' && crate.license === 'MPL-2.0') {
      const cssparser = crates.find(
        (p) => p.name === 'cssparser' && p.license === 'MPL-2.0',
      );
      docs.push(read(join(dirname(cssparser.manifest_path), 'LICENSE')));
    } else {
      const filename = upstreamFiles.get(repo);
      if (!filename) throw new Error('Review missing license: ' + crate.name);
      const revision = json(join(directory, '.cargo_vcs_info.json')).git.sha1;
      const url =
        repo.replace('github.com', 'raw.githubusercontent.com') +
        '/' +
        revision +
        '/' +
        filename;
      if (!upstreamCache.has(url)) {
        const response = await fetch(url);
        if (!response.ok) throw new Error('Cannot read ' + url);
        upstreamCache.set(url, await response.text());
      }
      docs.push(upstreamCache.get(url));
      if (repo.endsWith('/objc2')) {
        // The upstream summary links to MIT. Include its permission text and the crate authors.
        const mit = read('LICENSE');
        const terms = mit.slice(mit.indexOf('Permission is hereby granted'));
        const authors = crate.authors.length
          ? 'Authors (crate manifest): ' + crate.authors.join(', ') + '\n\n'
          : '';
        docs.push(authors + terms);
      }
    }
  }
  register(crate.name, crate.version, crate.license, source, docs);
}
const blocks = [...texts].map(
  ([content, id]) => '===== License text ' + id + ' =====\n\n' + content,
);
writeFileSync(
  'licenses/MACOS-NOTICES.txt',
  [
    'Directory UI Viewer — third-party notices for the Apple Silicon build',
    'Includes the declared frontend runtime dependencies, Tailwind CSS and the normal Rust dependency tree for ' +
      target +
      '. Package links provide corresponding upstream source. License expressions are as declared by the packages.',
    ...index,
    ...blocks,
  ].join('\n\n') + '\n',
);
console.log(
  'Wrote ' +
    index.length +
    ' package entries and ' +
    texts.size +
    ' distinct texts.',
);
