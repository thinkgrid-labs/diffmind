const fs = require('fs');
const path = require('path');

const version = process.argv[2];

if (!version) {
  console.error('Error: Please provide a version number (e.g., 0.2.0)');
  process.exit(1);
}

// Clean version string (remove 'v' prefix if present)
const cleanVersion = version.startsWith('v') ? version.slice(1) : version;

console.log(`Syncing version to v${cleanVersion}...`);

const filesToUpdate = [
  'package.json',
  'apps/local-cli/package.json',
  'packages/shared-types/package.json',
  'packages/core-wasm/package.json',
  'packages/core-wasm/Cargo.toml',
  'apps/local-cli/src/index.ts',
  'apps/local-cli/src/formatters.ts',
];

filesToUpdate.forEach(file => {
  const absolutePath = path.join(__dirname, '..', file);
  if (!fs.existsSync(absolutePath)) {
    console.warn(`Warning: File not found - ${file}`);
    return;
  }

  let content = fs.readFileSync(absolutePath, 'utf8');

  if (file.endsWith('package.json')) {
    // Update JSON version field
    content = content.replace(/"version":\s*"[^"]+"/, `"version": "${cleanVersion}"`);
  } else if (file.endsWith('Cargo.toml')) {
    // Update TOML version field (usually under [package])
    content = content.replace(/^version\s*=\s*"[^"]+"/m, `version = "${cleanVersion}"`);
  } else if (file.endsWith('.ts')) {
    // Update CLI commander version
    content = content.replace(/version\("[^"]+"\)/, `version("${cleanVersion}")`);
    // Update hardcoded versions (like in the banner)
    content = content.replace(/v[0-9]+\.[0-9]+\.[0-9]+/, `v${cleanVersion}`);
  }

  fs.writeFileSync(absolutePath, content, 'utf8');
  console.log(`✓ Updated ${file}`);
});

console.log('\nSuccess! All versions synced to v' + cleanVersion);
