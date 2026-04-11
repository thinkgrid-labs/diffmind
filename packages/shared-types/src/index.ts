/**
 * diffmind shared types
 *
 * Single source of truth for all output types produced by the ReviewAnalyzer
 * Wasm core and consumed by the CLI and (future) browser extension.
 */

// ─── Severity ─────────────────────────────────────────────────────────────────

/** Severity classification of a finding. */
export type Severity = "high" | "medium" | "low";

/** Ordered severity levels for sorting/filtering. */
export const SEVERITY_ORDER: Record<Severity, number> = {
  high: 0,
  medium: 1,
  low: 2,
};

// ─── Category ─────────────────────────────────────────────────────────────────

/** Category of a review finding. */
export type Category = "security" | "quality" | "performance" | "maintainability";

// ─── ReviewFinding ────────────────────────────────────────────────────────────

/**
 * A single finding detected in a git diff.
 * Matches the JSON structure returned by `ReviewAnalyzer.analyze_diff()`.
 */
export interface ReviewFinding {
  /** Relative file path, e.g. `src/auth/auth.controller.ts` */
  file: string;
  /** Line number in the diff where the issue was detected */
  line: number;
  /** Severity classification */
  severity: Severity;
  /** Category classification */
  category: Category;
  /** Human-readable description of the issue */
  issue: string;
  /** Concrete code fix suggestion */
  suggested_fix: string;
  /** Self-rated model confidence (0–1). Present when model supports it. */
  confidence?: number;
}

// ─── ReviewReport ─────────────────────────────────────────────────────────────

/** The full analysis result — an array of findings. Empty array = no issues. */
export type ReviewReport = ReviewFinding[];

// ─── Parsing Helpers ──────────────────────────────────────────────────────────

/**
 * Parse the raw JSON string returned by the Wasm core into a typed ReviewReport.
 * Returns an empty array if parsing fails (graceful degradation).
 */
export function parseReport(raw: string): ReviewReport {
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed as ReviewReport;
  } catch {
    return [];
  }
}

/**
 * Sort findings by severity (high → medium → low), then by file, then by line.
 */
export function sortFindings(findings: ReviewReport): ReviewReport {
  return [...findings].sort((a, b) => {
    const severityDiff = SEVERITY_ORDER[a.severity] - SEVERITY_ORDER[b.severity];
    if (severityDiff !== 0) return severityDiff;
    const fileDiff = a.file.localeCompare(b.file);
    if (fileDiff !== 0) return fileDiff;
    return a.line - b.line;
  });
}

/**
 * Filter findings by minimum severity level.
 * e.g. `filterBySeverity(report, "medium")` returns high + medium only.
 */
export function filterBySeverity(
  findings: ReviewReport,
  minSeverity: Severity
): ReviewReport {
  const threshold = SEVERITY_ORDER[minSeverity];
  return findings.filter((f) => SEVERITY_ORDER[f.severity] <= threshold);
}
