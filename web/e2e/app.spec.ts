// Drives the app the way a reader would: load and verify the snapshot, read
// an answer, open its receipt, try a what-if, switch questions.
// Needs RECEIPTS_SNAPSHOT_DIR and web/public/pkg (tools/wasm/build.sh).

import { expect, test } from "@playwright/test";

test.skip(!process.env.RECEIPTS_SNAPSHOT_DIR, "set RECEIPTS_SNAPSHOT_DIR to a built snapshot");

test("answers, receipts and what-ifs", async ({ page }, info) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));

  await page.goto("/");
  // The engine verifies every hash before showing anything.
  await expect(page.getByTestId("verified")).toContainText("Verified");
  await expect(page.getByTestId("verified")).toContainText(/threads?/);
  const threads = await page.getByTestId("verified").innerText();
  info.annotations.push({ type: "engine", description: threads });

  // The first question answers, and its leading bar opens a receipt whose
  // record count matches the bar.
  await expect(page.getByTestId("headline")).toContainText("slowest");
  const receiptRows = page.getByTestId("receipt-rows");
  await expect(receiptRows).not.toHaveText("0");
  const selectedBar = page.locator(".bar-row.selected .bar-count");
  await expect(selectedBar).toContainText(await receiptRows.innerText());
  await expect(page.getByTestId("step-0")).toContainText("behind");

  // Clicking another bar moves the receipt to it.
  const second = page.getByTestId("bar-1");
  await second.click();
  await expect(second).toHaveAttribute("aria-pressed", "true");
  const label = (await second.locator(".bar-label").innerText()).split(" · ")[0];
  await expect(page.getByTestId("receipt")).toContainText(label);

  // What-if: leave out a known issue; the answer is re-derived.
  const issue = page.locator('[data-testid^="issue-"]').first();
  await issue.check();
  await expect(page.getByTestId("whatif-note")).toContainText("Without");
  await expect(page.getByTestId("whatif-note")).toContainText(/incremental|rerun|unchanged/);
  await issue.uncheck();
  await expect(page.getByTestId("whatif-note")).toHaveCount(0);

  // Another question.
  await page.getByText("What do New Yorkers complain about most?").click();
  await expect(page.getByTestId("headline")).toContainText("top complaint");
  await page.screenshot({ path: info.outputPath("top-complaints.png"), fullPage: true });

  expect(errors).toEqual([]);
});
