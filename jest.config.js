/** @type {import('ts-jest').JestConfigWithTsJest} */
module.exports = {
  preset: 'ts-jest',
  testEnvironment: 'node',
  moduleNameMapper: {
    '^@diffmind/shared-types$': '<rootDir>/packages/shared-types/src',
  },
  testMatch: ['**/src/**/*.test.ts'],
  transformIgnorePatterns: [
    '/node_modules/(?!(ora|chalk|cli-progress)/)',
  ],
  collectCoverageFrom: ['**/src/**/*.ts', '!**/node_modules/**', '!**/dist/**'],
};
