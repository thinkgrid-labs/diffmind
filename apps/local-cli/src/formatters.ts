import chalk from "chalk";
import { type ReviewReport, type Severity } from "@diffmind/shared-types";

export const severityBadge = (severity: Severity): string => {
  switch (severity) {
    case "high":
      return chalk.red.bold(" ● HIGH ");
    case "medium":
      return chalk.yellow.bold(" ● MED  ");
    case "low":
      return chalk.blue.bold(" ● LOW  ");
    default:
      return chalk.gray(" ● ???  ");
  }
};

export const categoryBadge = (category: string): string => {
  return chalk.cyan(`[${category.toUpperCase()}]`);
};

export function formatMarkdown(report: ReviewReport, branch: string): string {
  if (report.length === 0) {
    return `### 🎉 No issues found against branch \`${branch}\`!`;
  }

  let output = `### 🧠 Diffmind Code Review — Analysis for \`${branch}\`\n\n`;
  output += `Found **${report.length}** finding(s).\n\n---\n\n`;

  for (const finding of report) {
    output += `#### ${finding.file}:${finding.line} — ${finding.severity.toUpperCase()}\n`;
    output += `**Category**: ${finding.category}\n`;
    output += `**Issue**: ${finding.issue}\n\n`;
    output += `\`\`\`typescript\n// Suggested Fix:\n${finding.suggested_fix}\n\`\`\`\n\n---\n`;
  }

  return output;
}

export function formatJson(report: ReviewReport): string {
  return JSON.stringify(report, null, 2);
}

export function parseReport(raw: string): ReviewReport {
  try {
    return JSON.parse(raw);
  } catch {
    return [];
  }
}

export function sortFindings(report: ReviewReport): ReviewReport {
  const severityOrder: Record<Severity, number> = {
    high: 0,
    medium: 1,
    low: 2,
  };

  return [...report].sort((a, b) => {
    if (a.file !== b.file) return a.file.localeCompare(b.file);
    if (severityOrder[a.severity] !== severityOrder[b.severity]) {
      return severityOrder[a.severity] - severityOrder[b.severity];
    }
    return a.line - b.line;
  });
}

export function filterBySeverity(report: ReviewReport, minSeverity: Severity): ReviewReport {
  const levels: Severity[] = ["low", "medium", "high"];
  const minIdx = levels.indexOf(minSeverity);

  return report.filter((f) => levels.indexOf(f.severity) >= minIdx);
}

export function printBanner(): void {
  console.log(chalk.cyan.bold("\n  diffmind") + chalk.dim(" v0.4.4 — local-first AI code review"));
  console.log(chalk.dim("  Model: Qwen2.5-Coder-1.5B-Instruct | Inference: Dual-Engine (Native + Wasm)\n"));
}
