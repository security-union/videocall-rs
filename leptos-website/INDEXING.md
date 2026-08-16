# Post-deploy indexing runbook

Run this checklist after deploying a new build of the marketing site to
production (https://videocall.rs). It gets the site re-crawled and confirms the
machine-readable layer (JSON-LD, llms.txt, sitemap, robots) is serving correctly.

## 1. Confirm the machine-readable files serve

- [ ] `curl -sI https://videocall.rs/llms.txt` returns `200` and
      `content-type: text/plain`.
- [ ] `curl -sI https://videocall.rs/llms-full.txt` returns `200` and
      `content-type: text/plain`.
- [ ] `curl -s https://videocall.rs/sitemap.xml` shows today's `<lastmod>`.
- [ ] `curl -s https://videocall.rs/robots.txt` lists the AI-crawler allow rules
      and the `Sitemap:` line.

## 2. Verify structured data (JSON-LD)

- [ ] Google Rich Results Test — https://search.google.com/test/rich-results —
      run against https://videocall.rs/. Confirm SoftwareApplication, FAQPage,
      and HowTo are detected with no errors.
- [ ] Schema.org validator — https://validator.schema.org/ — paste the page URL
      as a second check.

## 3. Submit the sitemap

- [ ] Google Search Console (https://search.google.com/search-console) →
      Sitemaps → submit `https://videocall.rs/sitemap.xml`.
- [ ] Bing Webmaster Tools (https://www.bing.com/webmasters) → Sitemaps →
      submit `https://videocall.rs/sitemap.xml`.

## 4. Request a re-crawl

- [ ] Google Search Console → URL Inspection → enter `https://videocall.rs/` →
      Request Indexing.
- [ ] Bing Webmaster Tools → URL Inspection / Submit URL for the home page.

## 5. IndexNow (optional, fast re-crawl for Bing / Yandex)

- [ ] Generate an IndexNow key and host it at
      `https://videocall.rs/<key>.txt`.
- [ ] Ping the changed URLs, e.g.:
      `curl "https://api.indexnow.org/indexnow?url=https://videocall.rs/&key=<key>"`.

## Notes

- The AI-crawler allow list lives in `public/robots.txt`. Update it when new
  bots appear.
- The JSON-LD graph lives in `src/app.rs` (`json_ld`). If site facts change,
  update the JSON-LD, `public/llms.txt`, and `public/llms-full.txt` together so
  the three stay in sync, and bump `dateModified` + the sitemap `<lastmod>`.
- `llms.txt` is the short index; `llms-full.txt` is the full content companion
  and is referenced from `llms.txt`.
