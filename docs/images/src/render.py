#!/usr/bin/env python3
#= render.py v1.0.0 — render an HTML mockup to a @2x PNG with Playwright.
#= Usage: render.py IN.html OUT.png
import logging
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

LOG = logging.getLogger("render")


def main(argv):
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    if len(argv) != 2 or not Path(argv[0]).is_file():
        LOG.error("usage: render.py IN.html OUT.png"); return 2
    src, out = Path(argv[0]).resolve(), Path(argv[1])
    with sync_playwright() as p:
        browser = p.chromium.launch()
        page = browser.new_page(viewport={"width": 760, "height": 400}, device_scale_factor=2)
        page.goto(src.as_uri())
        page.wait_for_timeout(300)  #= fonts and the outline script
        page.screenshot(path=str(out))
        browser.close()
    LOG.info("wrote %s", out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
