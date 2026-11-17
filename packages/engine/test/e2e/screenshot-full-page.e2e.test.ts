// Copyright (c) 2026 Cloudflare, Inc.
// Licensed under the Apache 2.0 license found in the LICENSE file or at:
// https://opensource.org/licenses/Apache-2.0

import { expect, test } from "./harness";
import { setContent } from "./set-content";
import { decodePng, type DecodedPng } from "./png";

const TALL = `<!doctype html><style>html,body{margin:0}div{height:100px}</style>
  <div style="background:#ff0000"></div><div style="background:#0000ff"></div>`;

const pixel = (png: DecodedPng, x: number, y: number) => {
  const i = (y * png.width + x) * 4;
  return [...png.data.slice(i, i + 4)];
};

// Puppeteer's fullPage sends captureBeyondViewport with no clip and expects the
// whole document, as Chrome captures it (cdp/page.ts, Page.captureScreenshot).
test("captureBeyondViewport without a clip captures the whole document", async ({ page }) => {
  await page.setViewport({ width: 64, height: 64 });
  await setContent(page, TALL);
  const full = await decodePng(new Uint8Array(await page.screenshot({ fullPage: true })));
  expect([full.width, full.height]).toEqual([64, 200]);
  expect(pixel(full, 32, 10)).toEqual([255, 0, 0, 255]);
  expect(pixel(full, 32, 190)).toEqual([0, 0, 255, 255]);

  await page.evaluate(() => window.scrollTo(0, 120));
  const scrolled = await decodePng(new Uint8Array(await page.screenshot({ fullPage: true })));
  expect([scrolled.width, scrolled.height]).toEqual([64, 200]);
  expect(pixel(scrolled, 32, 10)).toEqual([255, 0, 0, 255]);

  const viewport = await decodePng(new Uint8Array(await page.screenshot()));
  expect([viewport.width, viewport.height]).toEqual([64, 64]);
  expect(pixel(viewport, 32, 10)).toEqual([0, 0, 255, 255]);
}, 120_000);

// The pre-render drain lands the image, which grows the document from 64 to 104 px.
test("a full-page capture measures the document after the resource drain", async ({ page }) => {
  await page.setViewport({ width: 64, height: 64 });
  await page.goto("http://127.0.0.1:8787/__fixtures/xhr-boot.html", {
    waitUntil: "domcontentloaded",
  });
  await page.evaluate(() => {
    document.body.innerHTML =
      '<style>html,body{margin:0}</style><div style="height:40px;background:#0000ff"></div>' +
      '<img style="display:block;width:64px" src="/__fixtures/slow-headers-red.png?ms=1500">';
  });
  const full = await decodePng(new Uint8Array(await page.screenshot({ fullPage: true })));
  expect([full.width, full.height]).toEqual([64, 104]);
  expect(pixel(full, 32, 100)).toEqual([255, 0, 0, 255]);
}, 120_000);
