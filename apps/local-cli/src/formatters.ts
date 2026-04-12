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

export function printBanner(): void {
  console.log(chalk.cyan.bold("\n  diffmind") + chalk.dim(" v0.4.9 — local-first AI code review"));
  console.log(chalk.dim("  Inference: Dual-Engine (Native + Wasm)\n"));
}
