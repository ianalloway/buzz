import assert from "node:assert/strict";
import test from "node:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterContextProvider,
} from "@tanstack/react-router";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { ReadmePanel } from "./ProjectReadmePanel.tsx";

async function renderReadme(previewContent) {
  const router = createRouter({
    routeTree: createRootRoute(),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  await router.load();

  return renderToStaticMarkup(
    createElement(
      RouterContextProvider,
      { router },
      createElement(
        QueryClientProvider,
        { client: new QueryClient() },
        createElement(ReadmePanel, {
          file: { path: "README.md", previewContent },
        }),
      ),
    ),
  );
}

test("escaped examples in HTML README sections remain literal text", async () => {
  const html = await renderReadme(
    "<p>&lt;em&gt;example&lt;/em&gt; and &amp;lt;tag&amp;gt;</p>",
  );
  assert.ok(
    html.includes("&lt;em&gt;example&lt;/em&gt; and &amp;lt;tag&amp;gt;"),
  );
});

test("real emphasis, ampersands and inline code still render", async () => {
  const html = await renderReadme(
    "<p><strong>Fish &amp; Chips</strong> and <code>x &amp; y</code> with <code>&lt;item&gt; and `name`</code></p>",
  );
  assert.match(html, /<strong[^>]*>Fish &amp; Chips<\/strong>/);
  assert.match(html, /<code[^>]*>x &amp; y<\/code>/);
  assert.match(html, /<code[^>]*>&lt;item&gt; and `name`<\/code>/);
});

test("unconverted raw HTML stays text in the project preview", async () => {
  const html = await renderReadme("<sample-widget>example</sample-widget>");
  assert.ok(html.includes("&lt;sample-widget&gt;"));
  assert.ok(!html.includes("<sample-widget>"));
});
