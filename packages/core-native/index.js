const { existsSync } = require('fs');
const { join } = require('path');

const bindingPath = join(__dirname, 'core_native.node');

if (!existsSync(bindingPath)) {
  console.error(`Native binding not found at ${bindingPath}`);
  process.exit(1);
}

try {
  const binding = require(bindingPath);
  console.log('Successfully loaded native binding:', Object.keys(binding));
  module.exports = binding;
} catch (e) {
  console.error('Failed to load native binding:', e.message);
  process.exit(1);
}
