import {
  formatJson,
  formatMarkdown,
  severityBadge,
  categoryBadge,
} from "./index";
import { type ReviewReport } from "@diffmind/shared-types";

// Mock chalk to return plain strings for easier testing
jest.mock("chalk", () => {
  const createMock = () => {
    const fn = (str: string) => str;
    const proxy = new Proxy(fn, {
      get: (_target, prop) => {
        if (prop === "default") return proxy;
        return proxy;
      },
    });
    return proxy;
  };
  return createMock();
});

jest.mock("ora", () => {
  return jest.fn(() => ({
    start: jest.fn().mockReturnThis(),
    succeed: jest.fn().mockReturnThis(),
    fail: jest.fn().mockReturnThis(),
  }));
});

describe("cli formatters", () => {
  const mockReport: ReviewReport = [
    {
      file: "auth.ts",
      line: 10,
      severity: "high",
      category: "security",
      issue: "Hardcoded secret",
      suggested_fix: "Use env",
    },
  ];

  describe("formatJson", () => {
    it("should return formatted JSON string", () => {
      const output = formatJson(mockReport);
      expect(JSON.parse(output)).toEqual(mockReport);
    });
  });

  describe("formatMarkdown", () => {
    it("should return a summary message when no issues are found", () => {
      const output = formatMarkdown([], "main");
      expect(output).toContain("No issues found");
    });

    it("should return a formatted report when issues are found", () => {
      const output = formatMarkdown(mockReport, "feat-branch");
      expect(output).toContain("diffmind Code Review Report");
      expect(output).toContain("Branch: feat-branch");
      expect(output).toContain("auth.ts:10");
      expect(output).toContain("Hardcoded secret");
      expect(output).toContain("Fix: Use env");
    });
  });

  describe("badges", () => {
    it("should return correct severity badges", () => {
      expect(severityBadge("high")).toContain("HIGH");
      expect(severityBadge("medium")).toContain("MED");
      expect(severityBadge("low")).toContain("LOW");
    });

    it("should return correct category badges", () => {
      expect(categoryBadge("security")).toContain("SECURITY");
      expect(categoryBadge("quality")).toContain("QUALITY");
      expect(categoryBadge("performance")).toContain("PERF");
      expect(categoryBadge("maintainability")).toContain("MAINT");
    });
  });
});
