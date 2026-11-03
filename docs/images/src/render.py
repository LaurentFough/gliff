#!/usr/bin/env python3
#= render.py v1.1.0 — render an HTML mockup to a @2x PNG with Playwright.
#= Usage: render.py IN.html OUT.png [WIDTH HEIGHT]
#= WIDTH/HEIGHT are CSS points (default 760×400); the PNG is twice that.
import logging
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

LOG = logging.getLogger("render")


def main(argv):
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    if len(argv) not in (2, 4) or not Path(argv[0]).is_file():
        LOG.error("usage: render.py IN.html OUT.png [WIDTH HEIGHT]"); return 2
    try:
        width, height = (int(argv[2]), int(argv[3])) if len(argv) == 4 else (760, 400)
    except ValueError:
        LOG.error("WIDTH and HEIGHT must be whole numbers"); return 2
    if not (100 <= width <= 4000 and 100 <= height <= 4000):
        LOG.error("WIDTH and HEIGHT must be between 100 and 4000"); return 2
    src, out = Path(argv[0]).resolve(), Path(argv[1])
    with sync_playwright() as p:
        browser = p.chromium.launch()
        page = browser.new_page(viewport={"width": width, "height": height}, device_scale_factor=2)
        page.goto(src.as_uri())
        page.wait_for_timeout(300)  #= fonts and the outline script
        page.screenshot(path=str(out))
        browser.close()
    LOG.info("wrote %s (%dx%d pt)", out, width, height)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
