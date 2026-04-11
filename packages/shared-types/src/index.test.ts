import {
  parseReport,
  sortFindings,
  filterBySeverity,
  type ReviewFinding,
} from "./index";

describe("shared-types", () => {
  const mockFindings: ReviewFinding[] = [
    {
      file: "auth.ts",
      line: 10,
      severity: "high",
      category: "security",
      issue: "Hardcoded secret",
      suggested_fix: "Use environment variables",
    },
    {
      file: "utils.ts",
      line: 5,
      severity: "low",
      category: "quality",
      issue: "Shadowing variable",
      suggested_fix: "Rename variable",
    },
    {
      file: "auth.ts",
      line: 2,
      severity: "medium",
      category: "performance",
      issue: "Inefficient loop",
      suggested_fix: "Use map()",
    },
  ];

  describe("parseReport", () => {
    it("should parse valid JSON matching ReviewReport", () => {
      const json = JSON.stringify(mockFindings);
      expect(parseReport(json)).toEqual(mockFindings);
    });

    it("should return an empty array for invalid JSON", () => {
      expect(parseReport("invalid")).toEqual([]);
    });

    it("should return an empty array if parsed result is not an array", () => {
      expect(parseReport('{"issue": "test"}')).toEqual([]);
    });
  });

  describe("sortFindings", () => {
    it("should sort by severity (high > medium > low)", () => {
      const sorted = sortFindings(mockFindings);
      expect(sorted[0].severity).toBe("high");
      expect(sorted[1].severity).toBe("medium");
      expect(sorted[2].severity).toBe("low");
    });

    it("should sort by file and line within the same severity", () => {
      const sameSeverity: ReviewFinding[] = [
        { ...mockFindings[0], line: 20 },
        { ...mockFindings[0], line: 5 },
        { ...mockFindings[0], file: "a.ts" },
      ];
      const sorted = sortFindings(sameSeverity);
      expect(sorted[0].file).toBe("a.ts");
      expect(sorted[1].file).toBe("auth.ts");
      expect(sorted[1].line).toBe(5);
      expect(sorted[2].line).toBe(20);
    });
  });

  describe("filterBySeverity", () => {
    it("should filter findings by minimum severity", () => {
      const highOnly = filterBySeverity(mockFindings, "high");
      expect(highOnly).toHaveLength(1);
      expect(highOnly[0].severity).toBe("high");

      const mediumAndAbove = filterBySeverity(mockFindings, "medium");
      expect(mediumAndAbove).toHaveLength(2);
      expect(mediumAndAbove.map((f) => f.severity)).toContain("high");
      expect(mediumAndAbove.map((f) => f.severity)).toContain("medium");
    });
  });
});
