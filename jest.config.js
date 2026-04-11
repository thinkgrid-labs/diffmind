/** @type {import('ts-jest').JestConfigWithTsJest} */
module.exports = {
  preset: 'ts-jest',
  testEnvironment: 'node',
  moduleNameMapper: {
    '^@diffmind/(.*)$': '<rootDir>/packages/$1/src',
  },
  testMatch: ['**/src/**/*.test.ts'],
  transformIgnorePatterns: [
    '/node_modules/(?!(ora|chalk|cli-progress)/)',
  ],
  collectCoverageFrom: ['**/src/**/*.ts', '!**/node_modules/**', '!**/dist/**'],
};
